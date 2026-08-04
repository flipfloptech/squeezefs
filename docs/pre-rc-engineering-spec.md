# SqueezeFS — Pre-Release-Candidate Engineering Specification

**Date:** 2026-08-02 · **Tree:** reviewed against `dev` @ `68e8474`; Rev 3 status deltas verified against `dev` @ `391dec2` · **Revision 3**

This document specifies the engineering work required before the first release candidate. It is the output of a structured review of ~142 k LOC of source (`src/`, `crates/fuse3`, `crates/squeezefs-ipc`, `crates/squeezefs-preload`), 116 k LOC of tests, and the `.benchmarks/` and `docs/` record, conducted across nineteen independent read-only passes plus a three-part design workstream for the distributed lock manager.

**How to read this.** Each item is a work item with a stable ID, a component anchor (`file:line`), a statement of current behavior, a statement of required behavior, and acceptance criteria. Items are grouped by subsystem, not by discovery order. Priorities:

| Priority | Meaning |
|---|---|
| **P0** | Release-blocking. Data loss, silent corruption, a broken product feature, an unenforced access-control rule, or an unenforceable quality gate. |
| **P1** | Required before the scale target is claimed, or a correctness gap with a bounded blast radius. |
| **P2** | Robustness, accounting, conformance. |
| **P3** | Polish, documentation, cosmetics. |

**Verification status.** Items marked **[V]** were re-derived directly from source by the reviewer. Items marked **[A]** come from a domain pass with the evidence cited; they are high-confidence leads that should be confirmed by the owning engineer before the fix lands.

**Build gates as of this revision.** `cargo fmt --check` clean · `cargo clippy --all-targets --all-features` reports 0 warnings · `cargo test --no-run` clean · `cargo doc` clean. The clippy result is not meaningful as configured — see **ENG-1** — and a re-run with `--force-warn` surfaced 158 suppressed warnings.

---

## 0. Status summary

The codebase is strong where it was deliberately engineered: 54 loom models checked against *shipped* protocol cores, 1,529 tests, closed copy ledgers with byte-exact closure assertions, and a benchmark practice that publishes its own falsifications. Several subsystems were examined specifically for defects and none were found — `nt_copy`, `lease_core`/`cqe_core`/`wake_core`, the D0 evidence ladder, the KV journal framing and replay, the R-6 unified purge, the killpriv-v2 clearing law, `inode_pair_lock_order`, `SyncCoalescer`. Section 8 lists these as invariants to preserve.

The defects cluster in three areas, and all three share a root cause: **a rule that nothing mechanically enforces.**

1. **The quality gate is disabled.** `#![allow(clippy::all)]` at both crate roots means the authoritative gate inspects no clippy lints; `cargo audit` has never run; there is no CI.
2. **The durability contract is untested.** The tree contains a correct power-loss harness and points it at 100 % of the metadata plane and 0 % of the data plane. Every data-path crash test simulates process death over a surviving page cache. The benchmark fleet runs on substrates with no volatile write cache.
3. **The documented scale target and the shipped concurrency model differ.** The daemon takes an exclusive advisory lock on each metadata volume at mount, so the shipped model is one mount per volume set. Section 5 specifies the program to close this.

**Counts:** 17 P0 items, 46 P1, 61 P2, 38 P3.

### Rev 3 addendum — post-merge-train review (2026-08-02)

Five campaign branches merged to `dev` after the Rev 2 passes (29 commits, `68e8474..391dec2`: sdk-design, reset-v5-prep, derivation-sweep, microbench-program, volume-drain-flake). Consequences for this document:

- **Anchor drift.** Line-number anchors in `routing.rs`, `fuse_client.rs`, `checkpoint.rs`, and the fuse3 fork predate the merge train; re-anchor before cutting fix branches (the execution plan's Phase 0.4 sweep). The merged code itself (~2,500 new lines) is **unreviewed** by the nineteen passes.
- **FUSE-1 escalation [V], new evidence.** The finding now has a concrete casualty with a scheduled date: kernel-sqz **patch 0027** (`FUSE_TIME_LIMITS`, flags2 bit 62 — the generic/634 fix) consumes the daemon's echo from the **folded** capability word inside `process_init_reply` and does not modify the fold itself, so the fork's minor-31 / no-`FUSE_INIT_EXT` reply makes the kernel discard it. Unlike `FUSE_OVER_IO_URING` (module-param alternate path), bit 62 has no other consumption: **the reset-v5 window's generic/634-on-live-bit-62 row would test a structurally disengaged arm.** The daemon's four negotiation contract tests cannot catch this (kernel-side fold — the kernel-interface-only class). Ruled: execution-plan D6 (2026-08-02) chose the mainline-correct path — FUSE-1 pulled forward ahead of the window, kernel patch set frozen. Verified in-tree: `crates/fuse3/src/raw/abi.rs:37` (minor 31), `session.rs:1234-1237` (flags2 echo), the patch's `process_init_reply` hunk.
- **ENG-15 partially closed.** The Criterion-bench-count drift is gone (the microbench merge rewrote the AGENTS.md section and added the bench-baseline tier row). Still open: the stale service-thread `clamp(cpus/4,2,8)` figure, the unenforced `cargo audit` mandate (ENG-2), lock-order level 3.5 (RES-21).
- **TEST-8 further stale.** The three-suite gate's last green binary now trails dev by the five 2026-08-04 campaigns in addition to the drift recorded below.
- **PERF-14 re-confirmed in-class.** `block_allocator.rs:886` still pays `env::var` per call (`routing.rs` anchor drifted). Corroborating incident: the derivation sweep briefly introduced a per-checkpoint-tick resolver re-derivation (~20 allocs/tick) caught red by `ipc_op_economy_tests` and fixed at `open` — the standing lesson ("derived defaults resolve at admission/open/spawn, never on a cadence or per-op path") is now recorded in `.benchmarks/2026-08-04-derivation-sweep.md` finding 6.
- **TEST/E11 infrastructure landed.** `tests/run_bench_baseline.sh` merged with the microbench program; two post-merge fixes on `test/bench-baseline-pacing` (paced mode — one bench binary per quiet thermal window; the missing exec bit, whose piped invocation masked the failure as exit 0). The committed `reference.json` remains outstanding (inaugural save in progress).

---

## 1. Data integrity and durability

### DUR-1 · `fsync()` must make partially-covered striped blocks durable — **P0** **[A]**

**Anchor:** `src/fuse_client.rs:9692`, `:7554`, `:9643`, `:9673`, `:9707`; `src/tiering/nvme.rs:815`, `:1608`

**Current behavior.** `flush_inode_to_backend` calls `flush_memory_buffers_for_inode` first. For a block whose written-coverage union is partial, that takes the staging leg and calls `retire_parked_overlay` (`:7554`), emptying `active_block_buffers`. `flush_active_blocks_with_retry` (`:9643`) then builds its work list exclusively from that map, finds it empty, and returns at `:9673` — so `flush_one_active_block`, the function that DMAs staged bytes to the data device and merges the block map, is never invoked. The data-sync half (`:9707`) passes only the `file_id` key to `sync_key`; `active_block:` keys are never passed.

Two secondary findings in the same chain: `put_active_block` does call `msync`, but with `MS_ASYNC` (`tiering/nvme.rs:815`), which schedules writeback and issues no device flush; and `sync_key` (`:1608`) does not sync a key — it hashes the key to a shard and fsyncs the whole shard file, so even the `file_id` case depends on a hash coincidence. Separately, `sync_data_fut` and `sync_meta_fut` are joined with `tokio::try_join!` (`:9734`), i.e. concurrently, so there is no ordering edge between the data sync and the metadata barrier.

**Required behavior.** `fsync(fd)` must not return success until every byte acknowledged for that inode is on stable storage, for every layout. Collect the block index set before the memory-buffer flush (or union it with `list_staged_files()` filtered by the ino's `active_block` prefix), and have the staging leg escalate to `upload_active_block_bytes` rather than `put_active_block` + enqueue. The data barrier must complete before the metadata barrier that names it.

**Acceptance.** Write 1 MiB into a 4 MiB block, `flush_inode_to_backend`, then assert the block map names a published device key and `staged_writes_in_flight == 0` for that ino. Red against `:9673` today. Also required: the data-device power-loss harness from **TEST-1**, and a leg asserting the data barrier precedes the metadata journal entry.

**Note.** `tests/staged_crash_recovery_tests.rs` states this contract verbatim in its own header. It does not catch the violation because every leg is SIGKILL, where the staging file's page cache survives.

### DUR-2 · The data device must have a cache-flush primitive — **P0** **[V]**

**Anchor:** `src/nvme_dev.rs:101-116` (`UringRequest`), `:203-229` (open), `:340-380` (submit); `src/meta_backend/kv/backend.rs:1813`

**Current behavior.** `UringRequest` has exactly two variants, `Read` and `Write`, submitted with no `rw_flags`. There is no `Fsync` op, no FUA, no `O_SYNC`/`O_DSYNC`, and no VWC probe anywhere in the tree (`grep write_cache|id-ctrl|awupf` returns nothing). `NvmeBlockDev` exposes no barrier API. The `Fsync` opcode is used in `src/uring_fs.rs:812`, which serves metadata volumes and staging markers only. `fsync`'s sole barrier is `sync_device_for_ino` → `uring_fs::fdatasync` on the metadata volume path.

`O_DIRECT` bypasses the page cache, not the device's volatile write cache. The `O_DIRECT` open additionally degrades to buffered on failure (`:212-221`) with one `warn!` — and in that mode nothing ever flushes those writes, not `fsync(2)`, not clean unmount.

**Ordering consequence.** For striped write-through the sequence is DMA completes → block-map merge → meta journal → meta `fdatasync`. That barrier orders nothing on the data device. On power loss the result is durable metadata naming a block whose contents are still volatile — and because `close_rewrite_epoch` frees the displaced keys after the save and terminal frees queue a `BLKDISCARD`, the predecessor may already be gone.

**Substrate exposure.** Affected: real NVMe with VWC enabled (the default for essentially all enterprise and consumer devices), nvme-tcp with a file-backed or block-backed target, and file-backed volumes. Unaffected: zram, null_blk, tempfiles — which is the entire in-tree test and benchmark fleet.

**Required behavior.** Add an `Fsync { DATASYNC }` op to the `NvmeBlockDev` worker; issue it per touched data device inside `flush_inode_to_backend` before `persist_dirty_layout_if_needed`/`sync_device_for_ino`, with the same coalescing discipline as `sync_coalescer.rs`. Probe VWC at mount (sysfs `write_cache` and/or NVMe identify) and surface it as a stats gauge alongside `meta_volume_atomicity_physical`. The buffered fallback must either fail the mount or be covered by a real flush path.

**Acceptance.** Per-layout durability legs on the **TEST-1** harness: inline, staged, striped write-through, striped-via-staging, W1 patch, W2 fold, in-place overwrite, rewrite-shadow close, IPC ring write. Plus a no-orphaned-mapping assertion: after `fsync` + data-device power cut, every block key named by the durable map reads back the bytes written.

### DUR-3 · Reclamation watermarks must only advance on barriers that cover them — **P0** **[A]**

**Anchor:** `src/meta_backend/kv/backend.rs:1885` (`after_durable_barrier`), `:1813` (`sync_device`); `src/meta_backend/kv/checkpoint.rs:1148`, `:934`

**Current behavior.** `after_durable_barrier` unconditionally drains all of `pending_reclaim` after any successful barrier, with no epoch check, and it runs on every `sync_device()` for barrier leaders and followers alike.

The push at `checkpoint.rs:1148` is **unconditional** — outside the `if barrier_now` block at `:1151`. So in the steady state the cadence checkpoint (once per second, `barrier_now = false`) writes its ledger slot, pushes the tail, and returns, deferring durability to "the next barrier" — while nothing records which barriers are admissible. The next `fsync` or strict-mode commit to complete drains it, and that barrier's `fdatasync` was frequently submitted before the ledger write existed. `write_ledger_slot` is a buffered `uring_fs::write_at` whose own doc comment says "Durability rides the caller's barrier."

`SyncCoalescer`'s contract — "a caller is released only by a `sync_fn` that started after the caller registered" — is a guarantee about *the caller's own prior write*. It says nothing about state a third party pushed while the barrier was in flight. The coalescer is correct; this consumer misuses it.

**Consequences after power loss.**
- *Journal hole.* `advance_reusable_upto(tail)` lets new entries overwrite pages the older, still-selected ledger record needs. Replay's `seq == pos` identity correctly refuses the overwritten chain, then resyncs and applies the surviving suffix — entries N..N+k lost, N+k+1.. applied. Per-key LWW keeps each key internally sane; cross-key invariants do not survive.
- *Unmountable volume.* `alloc.advance_durable(tail)` releases pending-free extents whose freeing SMO is not durably covered; the extent is reclaimed and rewritten, and mount then refuses on the self-addressed `{node_addr, node_seq}` mismatch.

**Required behavior.** Stamp each `pending_reclaim` entry with a barrier epoch captured at push; have the coalescer's `sync_fn` publish the epoch it started at; drain only entries whose push-epoch precedes the completed barrier's start-epoch.

**Acceptance.** A leg that drives a cadence checkpoint concurrently with a `sync_device()` under the **TEST-1** harness and asserts the replay window is intact. Today no test combines `arm_power_cut` with the checkpoint task at all.

### DUR-4 · A failed bitmap page write must not discard the dirty set — **P0** **[A]**

**Anchor:** `src/meta_backend/kv/alloc_ext.rs:688-742`; caller `src/meta_backend/kv/checkpoint.rs:1057`

**Current behavior.** `write_dirty_pages` does `word.swap(0)` on the dirty bitmap first (`:690`), then every subsequent step is fallible (`encode_bitmap_page?`, `write_at_batch?`) with no restore. The error propagates out of `checkpoint_cycle` into `tick`, where it is a bare `log::warn!` (`checkpoint.rs:798`) — invisible per **ENG-3** — and the loop retries. Allocator deltas create no dirty-node floor, so the next successful cycle writes its ledger and advances the tail past the alloc/free records that were the only remaining copy. On remount the bitmap reports extents free while live nodes occupy them.

**Required behavior.** Snapshot-and-clear must be transactional: restore the bits (`fetch_or`) on every error path. The correct pattern is `restore_dying_floors` in the sibling path at `checkpoint.rs:1139`.

Related in the same function: the generation monotonicity guard at `:711-715` is `debug_assert!`-only, so in release a generation tie silently makes newest-valid-wins pick arbitrarily between the A and B slots. Promote it to a real check that refuses the write.

**Acceptance.** Fault-inject a bitmap-write failure, then verify a subsequent successful cycle persists those bits and the tail does not advance past the corresponding journal records.

### DUR-5 · The superblock needs redundancy and serialized updates — **P0** **[A]**

**Anchor:** `src/meta_backend/kv/superblock.rs:709-745`; runtime writers `src/meta_backend/kv/backend.rs:5756-5788`, `set_guest_slots_bit`, `set_volume_lifecycle_bit`, `set_slot_migration_bit`

**Current behavior.** Every other on-disk unit is A/B-rotated or CoW-appended. The superblock is a single 4 KiB `write_at(path, 0, img)` with no backup anywhere in the tree and no repair verb — a torn sector 0 makes the volume permanently unmountable, and on a 512e device or a file-backed volume the 4 KiB write is not atomic.

This is now reachable in steady state: `layout_deltas_ready()` stamps `KV_LAYOUT_DELTAS` on the first delta-class layout save of a fresh volume, i.e. during ordinary write traffic. `set_incompat_bit` is an unsynchronized read-modify-write, so two concurrent setters lose one bit — which defeats the "bit durable before the record it gates" ordering the bit exists to enforce. The doc comment at `:722-724` asserting the live backend never writes sector 0 is stale.

**Required behavior.** A/B the superblock (two sectors, generation + newest-valid-wins — the root-ledger pattern already in the tree), or write a redundant copy at a fixed tail offset and try it at mount. Serialize `set_incompat_bit` under a single lock or fold it into the checkpoint task.

**Acceptance.** Torn-write injection on both slots; a leg with two concurrent bit-setters asserting both bits survive.

### DUR-6 · The indirect block-map blob must be checksummed, CoW, and barriered — **P0** **[A]**

**Anchor:** `src/routing.rs:850-902` (encode/decode), `:3655-3699` (in-place reuse), `:3740-3759` (commit)

**Current behavior.** Three composed defects:

1. **In-place rewrite of live durable state.** `reuse_info` (`:3656`) rewrites the currently referenced blob block — the only non-CoW mutation in the metadata plane. A torn write destroys the previously committed map, not just the new one.
2. **No checksum.** The header is `magic(8) | version(4)` followed by a bare bincode `Vec<(u32, String)>`. Old and new images share an identical header, and entry offsets are stable across appends when key widths do not change — so a tear at a sector boundary can yield a new count with a stale tail that **deserializes cleanly into a plausible map with wrong block keys**. This is the only on-disk unit in the metadata plane without a digest, and the only silent-corruption path found there.
3. **No ordering barrier.** `nvme_writer.write_block` has no FUA or flush (see **DUR-2**), and nothing barriers the data device before `merge_layout_and_size`/`set_layout_and_size` commits and barriers the metadata device.

**Required behavior.** Version and checksum the whole image; allocate a fresh block per publish and free the old one after the layout commit is durable (the machinery exists — `old_indirect_to_free`); barrier the data device before the naming commit.

**Acceptance.** Torn-blob injection asserting refusal rather than a clean decode; a crash leg asserting the layout never names an undurable blob.

### DUR-7 · Cross-volume `nlink` and dentry must commit atomically — **P0** **[A]**

**Anchor:** `src/meta_backend/mod.rs:1591` (unlink), `:1690` (link), `:1982` (dir rename)

**Current behavior.** For metadata sets with more than one volume, link, unlink, and directory rename across parents each commit `nlink` on one volume and the dentry on another as two separate transactions, with no intent record, no compensating action on partial failure, and no crash recovery. The code's own comment on the rename case says "best-effort."

- link, crash between: `nlink = 2`, one dentry. `reclaim_orphaned_batch` skips `nlink > 0`, so the inode and all its blocks leak permanently.
- unlink, crash between: `nlink = 1`, zero dentries. Invisible and unreclaimable. Same leak.
- directory rename: parent `nlink` drifts permanently; `rmdir` then either succeeds with children present or refuses forever.

Same-volume paths are single whole-tx entries and are correct — this is purely the cross-volume composition. It contradicts the stated "whole-transaction atomicity by construction" contract for any multi-volume set, and it is untested (all suites run single-volume).

**Required behavior.** Make the operation a single distributed transaction, or refuse the cross-volume shape. Note this interacts with **DLM-*** below, which recommends more metadata volumes as the scaling axis.

**Acceptance.** A two-meta-volume test with a commit-boundary seam, asserting the pre- or post-state and never an intermediate one.

### DUR-8 · Additional metadata integrity items — **P1** **[A]**

| ID | Item | Anchor | Required behavior |
|---|---|---|---|
| DUR-8a | Extent-record checksum covers only `bytes[40..]`, excluding `fencing_token`, `block_idx`, `flags`, `count` — contradicting the type's own "checksummed as a unit" doc. A corrupted `block_idx` silently misattributes staged extents. The record is also rewritten in place, so a tear destroys the predecessor. | `src/cache/nvme.rs:605`, `:641`, `:1595` | Cover the header; make the rewrite CoW |
| DUR-8b | The layout-delta chain cap is a RAM-only counter reset on every cache refill, so the durable chain is bounded only by node compaction, not by the knob that claims to bound it | `src/routing.rs:3456`, `:3730`; `kv/backend.rs:5611` | Count deltas per key in the `xattr_slot` probe, or stamp chain depth into the delta wire |
| DUR-8c | `max_replayed_ino` scans only `TREE_INODES`, so a torn-dropped create followed by a surviving later entry on the same ino can let `next_ino` fall back and re-mint it | `kv/backend.rs:1069-1096` | Fold dentry-value and xattr-key inos into the watermark |
| DUR-8d | AEAD uses `Aad::empty()` — nonce uniqueness is sound, but with no positional binding a ciphertext block validates anywhere, so any stale or relocated mapping decrypts cleanly | `src/crypto_compress.rs:476`, `:549`, `:660` | Bind `AAD = (ino, block_idx, dev_offset)`; converts every stale-mapping class from silent wrong-data into a loud open failure at no cost |
| DUR-8e | `decompress_size_prepended` / `zstd::decode_all` allocate from an on-disk length field; on a compression-only volume nothing authenticates the payload | `src/crypto_compress.rs:376` | Bound the declared plaintext length by `block_size` before decompressing |
| DUR-8f | Four block-leak paths in `write_striped` and the staged-spill arm: `?` returns abandon an offset with refcount 1 that no map will name. The arm two lines away does free, so this is omission, not policy | `src/routing.rs:8807`, `:8816`, `:8496`, `:8519` | Free on every error exit |

---

## 2. Memory safety and lifetime management

### MEM-1 · Zero-copy read destinations must convey ownership — **P0** **[V]**

**Anchor:** `src/nvme_dev.rs:964-1019`

**Current behavior.**

```rust
let (buf_ptr, bytes) = if let Some(addr) = dest_addr {
    // SAFETY: destination address is pre-registered and pinned memory
    let b = unsafe { bytes::Bytes::from_static(std::slice::from_raw_parts(addr as *const u8, size)) };
    (addr as *mut u8, b)
} else {
    crate::cache::pool::read_bounce_pool(size).alloc()   // correct: ownership moves into the request
};
```

The pooled arm is correct — the `PooledBuf`'s `Bytes` moves into `UringRequest::Read`, so the worker owns the backing until it drops the request. The `dest_addr` arm conveys no ownership: `Bytes::from_static` over a non-`'static` registered transport payload buffer is lifetime laundering.

`:1003` then wraps the wait in `tokio::time::timeout(30s)`. On expiry the function returns `TimedOut`, the READ handler replies, and the transport issues `COMMIT_AND_FETCH` — re-arming that payload buffer for a new kernel request while the SQE is still in flight. When the device recovers, the DMA lands in a buffer owned by an unrelated request. Dropping the future has the same effect without any timeout.

A 30-second stall on a fabric-attached namespace is the condition the timeout exists to survive; as written the timeout converts a stall into cross-request corruption.

**Required behavior.** Give the zero-copy arm the same ownership property as the pooled arm — an owner token the worker holds that the ent re-arm path must wait on, or a per-`(qid, ent_idx)` in-flight epoch the COMMIT path refuses to re-arm past.

**Acceptance.** A leg that stalls the device past the timeout and asserts the payload buffer is not re-armed while an SQE references it.

### MEM-2 · Parallel assembly tasks must be owned, not detached — **P0** **[V]**

**Anchor:** `src/routing.rs:10252` (pointer capture), `:10283` (spawn), `:10308`–`:10429` (writes), `:10436` (join); same shape at `:8700`

**Current behavior.** The assembly destination is laundered to a `usize` — specifically to make the closure `Send`, which bypasses the `unsafe impl Send` review point that `RangedDest`, `SendPtr`, and `SendMutPtr` all go through with reviewed safety comments. One detached task is spawned per block; five `unsafe` copy sites write through the pointer, none with a safety comment. The join is `futures::future::try_join_all`.

`try_join_all` over `JoinHandle`s short-circuits on the first `JoinError` (panic or abort) and **drops the remaining handles, which detaches the tasks** — tokio does not abort on handle drop. Outer-future cancellation does the same, and the read path is explicitly documented as cancellation-prone (`InflightBlockReadGuard` exists to handle "FUTURE-DROP mid-fetch"). Inner `Err`s do not short-circuit (they are collected at `:10443`), which narrows the trigger set to panics and cancellation but does not close it.

Pooled arm: `final_buf` returns to `BUFFER_POOL` and is handed to another request while a task memcpys into it. Zero-copy arm: the ent payload is re-armed for a new kernel request.

**Required behavior.** Hold the tasks in a `JoinSet` (or an abort-on-drop guard) and join all of them before releasing the destination, collecting errors afterward; or give the tasks an owning handle so a late write hits memory that is still owned.

**Acceptance.** A leg that panics one block task and asserts the destination buffer is not recycled until every sibling has completed.

### MEM-3 · Cancellation safety in the zcrx classic lane — **P1** **[A]**

**Anchor:** `src/zcrx_lane/initiator.rs:776`, `:940-950`, `:799-813`

**Current behavior.** The `SendMutPtr` safety contract is stated as "the pointee outlives the op (the caller awaits the op's oneshot before releasing the buffer)." The timeout path honors it with an explicit abort-and-join; **cancellation is not handled**. On drop, the `Pending { dest, tx }` entry stays in `shared.pending` and no task is aborted, while the pooled `AlignedBufOwner` drops and recycles — after which `reader_loop` `read_exact`s into the recycled buffer.

The same path leaks CIDs: the `cid_gate` permit is RAII but the CID itself is only returned on the two normal exits, so after `queue_depth` cancellations the pool is empty while permits are available and the lane degrades to the kernel path for the mount lifetime.

**Required behavior.** A drop guard on `read_segment_classic` that performs the same poison + abort/join the timeout arm does, and CID return from a drop guard rather than the happy path.

**Acceptance.** A cancellation leg. **This lane must not ship default-on until this is closed.**

### MEM-4 · Unsound safe public APIs — **P1** **[V, confirmed by clippy]**

**Anchor:** `src/cache/pool.rs:363-375` (`UringBufOwner`), `src/routing.rs:2964-2975` (`RangedDest`), `src/zcrx_lane/area.rs:317-342` (`AreaSlice`)

Three `pub` types let safe downstream code trigger undefined behavior with no `unsafe` keyword: all-public fields, safe constructors, and safe accessors that dereference caller-supplied raw pointers. The `--force-warn` clippy run independently flags two of these as `not_unsafe_ptr_arg_deref` (`src/cache/pool.rs:478`, `src/zcrx_lane/fill_table.rs:61`).

**Required behavior.** Make the constructors `unsafe fn` with a stated contract, or make the fields private and the types `pub(crate)`.

### MEM-5 · `RwLockReadGuard<'static>` keep-alive defeated by field order — **P1** **[A]**

**Anchor:** `src/tiering/nvme.rs:264-269`, `:295-300`, transmutes at `:1217`, `:1268`

The safety comment states that the embedded `Arc<NvmeDevice>` keeps the lock alive. Rust drops struct fields in declaration order, and `_device` is declared **first** — so when the guard holds the last `Arc`, the device (which owns the `RwLock` and the `MmapMut` inline) is deallocated before `RwLockReadGuard::drop` runs. Reachable at cache teardown with a live guard.

**Required behavior.** Swap the two fields, or wrap the guard in `ManuallyDrop` and drop it explicitly first. One-line fix; the safety comment is correct in intent and ineffective as written.

### MEM-6 · `Vec::set_len` over uninitialized memory — **P1** **[V, confirmed by clippy]**

**Anchor:** `src/tiering/dht.rs:383-387`, `:396-400`

`set_len` publishes uninitialized bytes as initialized, then `&mut payload` forms a `&mut [u8]` over them. The length also comes from a peer-supplied `u32` fed to `with_capacity` before any cap check. Use `resize(n, 0)` and bound the length. (Note the whole DHT subsystem is currently unreachable — see **CLEAN-2** — so the pragmatic fix may be deletion.)

### MEM-7 · Contract and documentation gaps in `unsafe` code — **P2** **[A]**

| ID | Item | Anchor |
|---|---|---|
| MEM-7a | `PlacedSeverRegistry::sever`'s `# Safety` omits the page-alignment precondition that is the only release-build bound on the destination write; enforced by `debug_assert!` and by a screen in the single caller | `src/placed_sever.rs:91-106`; screen at `src/fuse_client.rs:6795` |
| MEM-7b | `map_shared_pmd_aligned` assumes a page-aligned `len`; a non-aligned caller would unmap the last page of the live mapping. Sound today only via an implicit cross-crate coupling | `crates/squeezefs-ipc/src/thp.rs:86-133` |
| MEM-7c | `ipc_direct`'s reaper error arm returns with `inflight_count > 0`, releasing the session mapping while kernel DMA may be in flight | `src/ipc_direct.rs:491-501` |
| MEM-7d | `std::ptr::read` on an alignment-1 `[u8; N]` for an 8-aligned `#[repr(C)]` struct; use `read_unaligned` or per-field `from_ne_bytes` | `src/fuse_client.rs:14674` |
| MEM-7e | ~165 of 585 `unsafe` sites have no `SAFETY:` comment within six lines. Concentrated in `routing.rs` (23 blocks / 4 comments) and `nvme_dev.rs` (9 / 1) — the payload-pointer sites where the comments matter most | — |

---

## 3. Input validation and access control

These are validation requirements, not vulnerability reports. Each states what the daemon currently accepts and what it must instead enforce.

### VAL-1 · GDS ioctl arguments must be range-checked — **P0** **[V]**

**Anchor:** `src/fuse_client.rs:14724-14726`; amplifier `src/routing.rs:7750`

**Current behavior.**

```rust
let end_offset = std::cmp::min(args.offset + args.size, file_size);
let start_block = (args.offset / block_size) as u32;
let end_block = ((end_offset - 1) / block_size) as u32;
```

`args` is read verbatim from caller memory. The add is unchecked, the release profile sets no `overflow-checks`, and the arm is **not** `#[cfg(feature = "gds")]`-gated — it is compiled into the default build. With `offset = 1, size = u64::MAX`, the add wraps to 0, `end_offset - 1` wraps, and `as u32` truncates to `0xFFFFFFFF`; `load_striped_block_keys` then executes `for b in 0..=u32::MAX { block_keys.push(...) }`, a `Vec<(u32, Option<String>)>` of roughly 137 GB. With `panic = "abort"` the allocation failure terminates the daemon.

The same loop is reachable without any overflow: a file `ftruncate`d to `max_file_size()` plus a whole-file request is 4.29 G iterations.

**Required behavior.** `checked_add` on `offset + size` returning `EINVAL`; early return when `end_offset == 0`; clamp `end_block` to the file's real block count; bound `load_striped_block_keys` by a per-call maximum; gate the arm behind the `gds` feature. If `gds` is enabled, `block_read_size` (`:14752`) and `dest_vram_address` (`:14754`) need the same treatment.

**Acceptance.** A test issuing the overflowing argument tuple and asserting `EINVAL` with no allocation; a fuzz target over the ioctl argument struct.

### VAL-2 · The reserved-xattr screen must be an allowlist — **P0** **[V]**

**Anchor:** `src/fuse_client.rs:14978` (`reserved_xattr_name`), `:14817`, `:14879`, `:14927`, `:14954`; records at `src/meta_backend/kv/backend.rs:5374`/`:5604`/`:5713`, `:230`, `src/fuse_client.rs:6924`, `:13437`

**Current behavior.** The screen is a denylist covering exactly two prefixes: `job:` and `user.squeezefs.`. Four internal records fall outside it and are therefore readable and writable through the FUSE xattr surface:

| Record | Anchor | What is exposed |
|---|---|---|
| `system.symlink` | `:13437` (write), `:13468` (read) | Every symlink's target. Note the Linux VFS deliberately performs no permission check for the `system.*` namespace — `xattr_permission()` returns 0 early with the comment "Decision on these is left to the underlying filesystem / security module." With no LSM active, `cap_inode_setxattr` ignores non-`security.` names, so the daemon is the only possible enforcement point and it performs none |
| `layout` | `kv/backend.rs:5374`, `:5604`, `:5713` | The per-inode block map, `block_prefix`, `file_id`, and wrapped data-key material. A write destroys the map; a write of a decodable value taken from another file redirects reads to that file's blocks |
| `writer_claim` | `kv/backend.rs:230`, `:2664`, `:3041` | The D0 single-writer guard record. Removing it presents the volume set as unclaimed to another host's takeover ladder |
| `client:{id}` | `fuse_client.rs:6924` | Daemon pid, mountpoint, and the job-wire endpoint. Removing them defeats the live-client refusal guarding `config set-cache-paths` |

That unprefixed names reach the FUSE boundary is established by the existence of the `job:` screen itself, whose comment records the pre-VL2 exposure as "a real hole." The daemon's own tests write layouts through this surface (`tests/extent_patch_tests.rs:939`).

`options.default_permissions(true)` (`:15260`) means these writes require the corresponding VFS permission on the object — which is satisfied on mounts presented with `--uid <service-user>` and on shared-scratch filesystems.

**Required behavior.** Convert `reserved_xattr_name` to a positive allowlist: permit `user.*` (minus `user.squeezefs.`), `security.*`, and `trusted.*`; refuse everything else with `EPERM` and count it. Mirror the screen inside `KvMetaBackend::{set,get,list,remove}xattr` so the FUSE layer is not the only enforcement point. Filter refused names from `listxattr`. Consider relocating internal records out of the xattr keyspace entirely.

**Acceptance.** A test per internal record asserting `EPERM` on set/remove, `ENODATA` on get, and absence from `listxattr`, exercised through the FUSE surface and directly against the backend.

### VAL-3 · The encryption key must not be stored on the volume it protects — **P0** **[V]**

**Anchor:** `src/main.rs:106-108` (CLI), `:3506` (store), `src/lib.rs:536` (`FormatConfig.encrypt_key`), `src/config_ops.rs:832` (persist), `src/fuse_client.rs:11476` (consume), `src/crypto_compress.rs:161`

**Current behavior.** `--encrypt-key` is documented as *"Path to RSA private key PEM file"*. The value is stored verbatim into `FormatConfig.encrypt_key`, persisted as the `user.squeezefs.format_config` xattr on ino 1, and consumed as `RsaPrivateKey::from_pkcs1_pem(pem)` — i.e. as PEM **content**. Nothing anywhere reads the file.

Two consequences:

- **As documented, the feature does not function.** Passing a path makes `from_pkcs1_pem("/etc/key.pem")` fail, leaving `private_key = None` and `precomputed_encrypt_key = None`, so every `resolve_encrypt_key()` on a transformed volume errors and all writes fail.
- **The only working usage stores the key material in cleartext on the metadata volume it encrypts**, and passes it on `argv` (visible in `/proc/<pid>/cmdline` for the duration of `format`). At-rest encryption then provides no protection against the threat it exists for.

**Required behavior.** Read the PEM from the path with `O_NOFOLLOW` and a mode check, or accept it on stdin. Persist only a KDF salt or key identifier in `FormatConfig`; resolve the key at mount from a file, keyring, or KMS. Add `#[serde(skip)]` and a redacting `Debug` impl on any key-bearing field.

**Acceptance.** A test asserting `format --encrypt-key <path>` produces a mountable, writable encrypted volume, and that no key material appears in the persisted format config.

### VAL-4 · The shim must authenticate the daemon it connects to — **P0** **[A]**

**Anchor:** `crates/squeezefs-preload/src/session.rs:360-434`; `src/ipc_host.rs:2224-2252`

**Current behavior.** The shim reads a `BootstrapBlob` from the mount via `fgetxattr`, connects to the socket the blob names, sends the caller's file descriptor over `SCM_RIGHTS`, receives a memfd, and maps it. `grep -n 'SO_PEERCRED\|F_GET_SEALS' crates/squeezefs-preload/src` returns nothing: the shim never checks who is on the other end and never verifies the memfd's seals.

The daemon's validation of the client is thorough (ABI, build commit, nonce, `SO_PEERCRED`, fd screen, budget, per-uid cap). The client-side ladder does not exist. Because the blob — including the socket name it dictates — is read from the mount itself, any filesystem that can answer that `fgetxattr` determines the rendezvous.

Related, same subsystem: for non-root mounts without `XDG_RUNTIME_DIR`, the path socket lands in `/tmp/squeezefs-il-<uid>/` (`src/fuse_client.rs:15404`), created by `path_listen` (`ipc_host.rs:2225`) with `create_dir_all` and no ownership or mode check, and the socket itself is set to `0666`. The abstract socket name is unguessable and is not the concern; the filesystem rendezvous is.

**Required behavior.** Before sending the credential fd: `getsockopt(SO_PEERCRED)` on the socket and require `uid == 0 || uid == st_uid` of the mount root (already `fstat`ed at `interpose.rs:381`); require the path rung to resolve under a root-owned directory; `fcntl(memfd, F_GET_SEALS)` requiring `F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW`. Daemon side: create the socket directory with an ownership and mode check (`st_uid == geteuid() && (st_mode & 0o022) == 0`), bind via a dirfd rather than a path, and refuse rather than falling back to `/tmp`.

**Acceptance.** A leg where the shim is pointed at a socket owned by a different uid and asserts refusal plus passthrough.

### VAL-5 · IPC control-plane resource bounds — **P0** **[A]**

**Anchor:** `src/ipc_host.rs:2374-2391` (`recv_ctl`), `:1276` (`connection_loop`), `:1235-1270` (accept), `:743` (`drain`), `slot_core.rs:163-183`

Five bounds are missing on a socket reachable by any process in the namespace:

| ID | Current behavior | Required behavior |
|---|---|---|
| VAL-5a | `recv_ctl` copies one `RawFd` per `SCM_RIGHTS` cmsg and never consults `cmsg_len`; the 64-byte control buffer admits ~12, and the remainder are installed in the daemon and never closed. `MSG_CTRUNC` is never tested. This runs before any validation | Derive the count from `cmsg_len`, wrap every fd in an `OwnedFd`, refuse the datagram if the count is not 1 or if `MSG_CTRUNC` is set |
| VAL-5b | `connection_loop` blocks on the first datagram with no `SO_RCVTIMEO`, is not registered in `admin_conns`, and its thread is unconditionally joined by `shutdown()` — so a connection that sends nothing prevents daemon teardown indefinitely | Set `SO_RCVTIMEO`; register every accepted connection in a registry that `shutdown()` shuts down |
| VAL-5c | `try_begin_serve` succeeds for any slot whose state word reads `STATE_SUBMITTED`; that word is client-writable, and there is no in-flight counter anywhere in the host and no admission gate in `serve_write`. Honest in-flight is bounded by `slots`; a re-submitted slot is not. Each op severs up to `max_op_bytes` (1 MiB default), and `SeveredPool` caps retention, not allocation | A per-session in-flight counter incremented at dequeue and decremented in `SlotCompletion::complete` and every reject path; exceeding `slots` is a protocol violation and should poison the session |
| VAL-5d | `accept4` spawns an unbounded OS thread per connection before any validation, and `self.threads` is push-only — one `JoinHandle` retained per connection ever accepted. The removal pattern exists 160 lines away (`admin_conns` uses `retain`) | Cap concurrent control threads; `retain(|h| !h.is_finished())` on each accept |
| VAL-5e | `IpcSession::drain` is `while let Some(index) = consumer.pop(&ring)` with no per-pass budget. Sessions are pinned to a service thread and `service_loop` iterates them serially, so one session that keeps its ring non-empty starves every other session on that thread | Cap the drain at N ops per session per pass and round-robin |

### VAL-6 · Job-wire configuration and framing — **P0** **[A]**

**Anchor:** `src/main.rs:4662-4676`, `src/job_wire.rs:190`, `:213`, `:220-229`, `:714-742`, `:829-856`; `src/tiering/dht.rs:179-205`

**Current behavior.**

- Every write mount starts a TCP listener on `0.0.0.0` (the `JobWireConfig::default()` loopback bind is overridden at `main.rs:4663`). `security` is hardwired `None`; `grep -n security src/main.rs` returns two hits, both comments — there is no flag, env var, or config field that can populate `ClusterSecurityConfig`, so the warning the listener emits recommends a configuration the binary cannot express. There is no way to disable the listener or bind it to loopback.
- With `ca_cert: None`, the client config installs an accept-everything certificate verifier via `.dangerous()` and the server uses `with_no_client_auth()`; the server name is the literal `"localhost"`. Because the verification-strength ladder keys on `transport == "tls"` (`:724`), this configuration is nonetheless permitted to sample verify-reads below 100 %.
- The enrollment proof is `HMAC(secret, worker_id ‖ endpoint_nonce ‖ "hello")` where the *worker* chooses the nonce; there is no nonce registry, freshness window, or server challenge, and no per-frame authentication after enrollment.
- `read_frame` allocates `vec![0u8; len]` (up to `MAX_FRAME_BYTES`, 16 MiB) immediately after the length prefix and before `read_exact`; the 10 s timeout covers only the HELLO frame. Concurrent connections are uncapped, `handles` is push-only, and the `accept` error arm `continue`s with no backoff (an `EMFILE` condition becomes a busy loop).

**Required behavior.** Decide the posture explicitly: either (a) add a `--job-wire-bind` / `--no-job-wire` surface plus a real TLS configuration path and default the bind to loopback, or (b) do not start the listener unless remote workers are configured. Key the verification-strength ladder on CA-pinned mTLS, never on the presence of a TLS object; refuse a `ClusterSecurityConfig` with no CA. Add a coordinator-issued enrollment challenge with a nonce registry before any mutating job type becomes `wire_executable`. Cap concurrent connections, back off on accept errors, defer body allocation until post-enrollment or stream it in bounded chunks, and prune finished handles.

`mac_eq` (`:251-259`) is already a correct constant-time comparison and should be preserved.

### VAL-7 · Additional validation and access-control items — **P1/P2** **[A]**

| ID | Item | Anchor | Priority |
|---|---|---|---|
| VAL-7a | `.stats` and `.config` virtual inodes are mode `0444` and emit every cached block key, the read-cache census, `active_writes` keyed by inode, every device path, and every staging directory. `open()` regenerates the full payload each time | `src/fuse_client.rs:5185`, `:6322`, `:5359` | P1 — mode `0400` owned by the mount uid; move the key census behind an opt-in |
| VAL-7b | Staging and read-cache segment files and directories are created with default modes (0644 / 0755). On passthrough volumes these hold plaintext user file data. `stamp_staging_dir` additionally `chown`s through a symlink-following call | `src/tiering/nvme.rs:397`, `src/cache/nvme.rs:95`, `:938`; `src/config_ops.rs:115-121` | P1 — `0700`/`0600`; dirfd-relative create and `fchown` |
| VAL-7c | The admin lane checks `SO_PEERCRED` identity correctly but skips the ABI, build-commit, and nonce checks the data plane enforces. `owner_uid` derives from `SUDO_UID` | `src/ipc_host.rs:1380-1402`; `src/config_ops.rs:81` | P1 — apply the same ladder; make the admin uid an explicit flag |
| VAL-7d | No per-user quota, rate limit, or accounting anywhere in the FUSE path; every R5 component is process-global, so one workload can drive the budget to Red, which pauses maintenance, tier publishes, and dehydration for all users | — | P1 — either implement per-uid accounting or state the single-tenant posture explicitly in `docs/operations.md` |
| VAL-7e | `copy_file_range` runs an O(file-size) per-block probe loop four times per call, with an unbounded result vector | `src/fuse_client.rs:13946-13975` | P2 — bound the scan to the range being copied |
| VAL-7f | The GDS ioctl reads `/proc/<req.pid>/mem` with no pid-liveness check and no PID-namespace translation | `src/fuse_client.rs:14654-14675` | P2 — use `process_vm_readv` with a pidfd, or the kernel's restricted-ioctl retry protocol |
| VAL-7g | `validate_nqn_component` rejects `/` and whitespace but not `.` or `..`; the consumer's `ENOTEMPTY` fallback is `remove_dir_all` on a configfs path | `src/nvmeof/mod.rs:295`, `src/nvmeof/nvmet.rs:190` | P2 — reject `.`, `..`, and leading-dot components |
| VAL-7h | No `zeroize` on key material, no `PR_SET_DUMPABLE(0)`, no `RLIMIT_CORE`; the log file is created 0644 with no `O_NOFOLLOW` | `src/main.rs:2576`, `:2829`; `src/crypto_compress.rs:133-137` | P2 |
| VAL-7i | `verify_checkout` validates `HEAD` against the pinned SPDK commit but not the worktree state, so a modified checkout at the pinned commit builds as root. All root subprocesses rely on inherited `$PATH` | `src/nvmeof/spdk/lifecycle.rs:796-812` | P3 — add `git status --porcelain` / `git diff --quiet HEAD`; use absolute paths |

The SPDK commit pin (`:905-914`), the 0700 run-dir / 0600 RPC socket, the argv-only subprocess discipline (all 63 sites), and `default_permissions(true)` are correct and are listed in section 8 as invariants.

---

## 4. Interface conformance

### FUSE-1 · INIT must negotiate `flags2` correctly — **P0** **[V]**

**Anchor:** `crates/fuse3/src/raw/session.rs:4891` (`negotiate_reply_flags`), `:1273` (reply), `abi.rs:37` (`FUSE_KERNEL_MINOR_VERSION`), `abi.rs:178`

**Current behavior.** The reply sets `flags2 = FUSE_OVER_IO_URING_FLAGS2` but never sets `FUSE_INIT_EXT` in `flags`. Mainline `process_init_reply()` folds `arg->flags2` only when `arg->flags & FUSE_INIT_EXT` **and** `arg->minor >= 36`; the fork pins `FUSE_KERNEL_MINOR_VERSION = 31`. So `flags2` is discarded for two independent reasons, and **fixing `FUSE_INIT_EXT` alone will not work.**

Consequence today: `fc->io_uring` remains 0, so the kernel's `fuse_block_alloc()` arm-window gating never engages — the mechanism intended to shrink the classical straggler window during arm. The transport functions because `fuse_uring_cmd()` gates REGISTER on the module parameter rather than the negotiated bit, i.e. through a path the kernel does not consider negotiated. This is a plausible contributor to the request-stall class the classical sideband and the 30 s REGISTER barrier were built to survive.

The version pin also makes `SETXATTR_EXT` (7.33), `CREATE_SUPP_GROUP` (7.34), `PASSTHROUGH` / `HAS_RESEND` / `SECURITY_CTX` (7.36), and `DIRECT_IO_ALLOW_MMAP` (7.39) permanently unreachable.

**Rev 3 escalation.** This finding now blocks scheduled work: kernel-sqz patch 0027's `FUSE_TIME_LIMITS` (flags2 bit 62, the generic/634 fix staged for the reset-v5 window) is consumed from the folded word in `process_init_reply` and the patch does not alter the fold — so the daemon's echo is discarded for the same two reasons, and bit 62 has no module-param-style alternate path. The reset-v5 window's Phase 8 row is disengaged until either this item lands or the patch folds `flags2` unconditionally (see the execution plan's D6).

**Required behavior.** Raise `FUSE_KERNEL_MINOR_VERSION` to ≥ 36 and set `FUSE_INIT_EXT` whenever `init_in.flags & FUSE_INIT_EXT != 0` and `flags2 != 0`, in the same change.

**Acceptance.** A negotiation test asserting both the version and the bit ride every reply carrying a nonzero `flags2`. Note the existing `kernel_init_folds_flags2_into_high_bits` test pins the *inbound* fold only. Because raising the minor version changes what the kernel expects, A/B the change against the target kernel before landing.

### FUSE-2 · Every request must produce exactly one reply — **P0** **[A]**

**Anchor:** see table

**Current behavior.** Eleven paths consume a request without producing a reply. A lost reply leaves the calling application in uninterruptible sleep and makes `umount` return EBUSY. Only two paths have any detector, and both are `transport_debug`-gated (off in production).

| # | Path | Anchor |
|---|---|---|
| 1 | Handler task panic — `spawn_local`, `JoinHandle` dropped, panic captured and discarded | `session.rs:4726-4731` |
| 2 | `reply_error_in_place`'s own send is `let _ = …send()`, so after the reply task exits every error reply is discarded | `session.rs:4609` |
| 3 | Reply-task death on any non-`NotFound` write error ends the worker session while dispatched handlers keep running | `session.rs:598-635` |
| 4 | `InboundQueue::push` on a closed receiver is `let _ = self.tx.send(req)`, while the worker keeps inserting into `pending` | `fuse_over_uring.rs:549` |
| 5 | CQE-error reclaim `retain`s the pending entry away without synthesizing a reply | `:2441-2448` |
| 6 | `EAGAIN`/`EINTR` on a COMMIT re-REGISTERs instead of re-committing, discarding the reply `apply_reply` already wrote. `user_data == ent_idx` for both ops, so the worker cannot distinguish them — the code states this hazard 60 lines later | `:2421`, cf. `:2489` |
| 7 | Six `let _ = push_cmd_batched/push_poll_batched` sites. Losing the poll re-arm stops the queue waking on `commit_tx` entirely | `:2505`, `:2647`, `:2653`, `:2719` |
| 8 | `shutdown()`'s unconditional `pending.clear()`, reachable from non-fatal causes (`connection_watch` returning `EBADF`, a kernel protocol reject, any worker `Err`, `Drop`) | `:1674-1690` |
| 9 | Post-shutdown replies take the classical path because `write_vectored` gates on `is_ready()`, and the classical write returns `ENOENT` | `tokio.rs:728` |
| 10 | `pending.insert` overwrites on unique collision | `:2630` |
| 11 | `fuse_resend` / `fiq->ops` switchover double-delivery — needs a live mount to confirm | `tokio.rs:705`, `:746` |

**Why it cannot be asserted today.** `pending` has three removal paths — `remove` (reply), `retain` (CQE error), `clear` (shutdown) — and only the first submits a commit. There is no per-ent request-liveness state machine; `lease_states[]` tracks payload aliasing, not liveness.

**Required behavior.** State the invariant: *for every `unique` inserted into `pending`, exactly one COMMIT_AND_FETCH carrying either a reply or a synthesized error is submitted before the entry leaves the map, and the entry leaves the map only by that submission.* Implement:

- A per-`(qid, ent_idx)` slot state (`Registered → Delivered → Replied → Registered`) owned by the queue worker, with the unique map as a lookup index rather than the source of truth. (This composes with **PERF-16**, which proposes deleting the map for throughput reasons.)
- One `fail_ent(ent, errno)` helper routed from every non-reply exit. The machinery already exists inline twice (`:2500-2515`, `:2706-2718`).
- Always-on counters `transport_requests_failed_synthetic` and `transport_requests_abandoned` (must stay 0), and promotion of the `:1973` stale-pending scan to an ungated watchdog.
- For row 6: distinguish op class in `user_data` and re-push the same `CommitMsg`.
- For row 10: refuse the insert on collision, log loudly, and force an `EIO` commit on the displaced ent.

**Acceptance.** A leg per row where reachable in-process; `transport_requests_abandoned == 0` across the full suite; the `fuse_resend` case on a live mount.

### FUSE-3 · Transport robustness items — **P1** **[A]**

| ID | Item | Anchor | Required behavior |
|---|---|---|---|
| FUSE-3a | Non-fatal CQE errors (`ENOMEM`, `EBUSY`, `EPERM`, `EFAULT`) drive an unthrottled re-REGISTER loop with no backoff, no cap, and one `warn!` per iteration | `fuse_over_uring.rs:2441` | Bounded retry with exponential backoff; retire the ent after K failures; fail the session if all ents retire |
| FUSE-3b | `submit_reply` returns `Ok(len)` on `NotFound`, so a genuinely dropped reply is counted as delivered on the stats inode | `tokio.rs:775-786` | Count it separately; keep it out of `STATS_REPLIES` |
| FUSE-3c | The REGISTER barrier counts submissions, not completions, so `kmbuf_negotiated` can be set for a session whose REGISTERs were refused | `:2241-2243`, `:1505` | Count a queue registered only after `depth` successful REGISTER CQEs are reaped |
| FUSE-3d | `apply_reply` silently truncates an oversize reply and reports the truncated `payload_sz` while `fuse_out_header.len` still claims the full size | `:2784-2807` | Emit the header-only `-EIO` shape used for other degenerate cases; count it |
| FUSE-3e | A second parked commit for one ent overwrites the first in release builds (`debug_assert` only). The two delivery-time aliasing invariants at `:2499`, `:2554`, `:2603` are also `debug_assert`-only | `:2326` | Loud-never-fatal: log, commit `EIO`, count |
| FUSE-3f | No CQ-overflow detection; `IORING_FEAT_NODROP` never probed; the overflow counter never checked. CQ is `sq*2` while a pass can push `depth + depth + 1` | `:1786`, `:2377` | Probe the feature word; check overflow after `cq.sync()`; count |
| FUSE-3g | `data_ref` is sized from `in_header.len` rather than the bytes the transport actually filled, and underflows if `len < 40`. A clamped delivery hands handlers stale bytes from the previous request on the same loop | `session.rs:875`; `tokio.rs:632-665` | Validate `len >= 40 && data_size <= filled`; reply `EINVAL` otherwise |
| FUSE-3h | `classical_inflight` never removes `FUSE_NOTIFY_REPLY` (opcode 41) uniques; one leaked entry makes every reply on every queue take the shared set mutex | `tokio.rs:705`, `:170` | Exclude 41 alongside 2 and 42 |
| FUSE-3i | The daemon replies to INTERRUPT, which the protocol defines as no-reply; `fc->no_interrupt` therefore never latches | `session.rs:3737-3777` | Do not reply |
| FUSE-3j | Shutdown lease drain uses a 1 ms sleep-poll up to 100 ms per parked ent, serially — up to 3.2 s per queue at depth 32 — and then discards the reply body | `:2693-2698` | Use the existing eventfd wake |
| FUSE-3k | `BATCH_FORGET` discards `nlookup`; the daemon keeps no lookup refcount and treats each forget as one unconditional eviction | `session.rs:4118`; `fuse_client.rs:14612` | Carry `nlookup` through and honor it |
| FUSE-3l | Two unchecked slice-splits in the dispatch task (not a handler task): a short `NOTIFY_REPLY` or `BATCH_FORGET` panics the whole session | `session.rs:4032`, `:4087` | Bounds-check |

### FUSE-4 · Negotiated capabilities must match implemented semantics — **P1** **[A]**

| ID | Flag / limit | Finding | Required behavior |
|---|---|---|---|
| FUSE-4a | `FUSE_WRITEBACK_CACHE` negotiated but `FOPEN_KEEP_CACHE` never set | The kernel invalidates the page cache on **every open**. `FUSE_AUTO_INVAL_DATA` is already negotiated, which is what makes setting `KEEP_CACHE` safe | Set it in `regular_open_reply_flags()` (`fuse_client.rs:4039`); measure with a warm double-read |
| FUSE-4b | `FUSE_EXPORT_SUPPORT` advertised unconditionally | `generation` is hardcoded `1` on every reply, and `..` resolution `.unwrap_or(1)`-fabricates root on failure — which the lookup layer explicitly promises never to do. A reformat reissues the same inos with generation 1, so a pre-reformat handle resolves to a different file instead of `ESTALE` | Derive a real generation (the superblock uuid is already the volume-set generation identity) or gate the flag behind an explicit mount option |
| FUSE-4c | `FUSE_CACHE_SYMLINKS` advertised | No invalidation path exists for cached targets | Pair with **POSIX-3** |
| FUSE-4d | `max_readahead` echoed verbatim | Never consulted; the R2 prefetch window is derived independently | Either couple them or document the independence |
| FUSE-4e | Reply-side `max_pages` | `get_payload_buffer` returns `(ptr, len)` and the length is discarded; `RangedDest.cap` is set from the requested slice length. The only bound on the destination write is the kernel honoring `max_pages` — an unchecked cross-ABI invariant | Thread the returned size through as `cap` and refuse any serve exceeding it |

Correctly implemented and verified: `ATOMIC_O_TRUNC`, `PARALLEL_DIROPS` (shared parent lock plus Δtime merge records), `MAX_PAGES` (geometry-derived), `HANDLE_KILLPRIV_V2`, `time_gran`, `DONT_MASK` (correctly never advertised), and the deliberate non-advertisement of splice, POSIX locks, flock, and POSIX ACLs — each with a recorded rationale and a pinning test.

---

## 5. POSIX semantics

| ID | Item | Anchor | Priority | Required behavior |
|---|---|---|---|---|
| POSIX-1 | `statfs.f_ffree` derives from the monotonic ino watermark, so `IUsed` rises forever and never recovers on delete. A create/delete loop reports a full filesystem on an empty one, and tools gating on `IUse%` will refuse to write | `src/fuse_client.rs:14297-14319` | P1 | Maintain a live `live_inodes` gauge — `destroy_inodes` already knows the count it removes |
| POSIX-2 | Sparse files are entirely invisible: no `lseek` handler at all (so `SEEK_HOLE` returns EOF and `SEEK_DATA` returns the offset), and `st_blocks` is synthesized from size. The filesystem genuinely supports holes; the information exists and is never exported. `cp --sparse`, `tar -S`, `rsync -S`, and `qemu-img` all degrade to full-size copies | `src/fuse_client.rs:10889`; `lseek` absent | P1 | Implement `lseek` for `SEEK_HOLE`/`SEEK_DATA`; derive `st_blocks` from the block map |
| POSIX-3 | `lstat()` on a symlink returns `st_size == 0` once the attr TTL lapses — `symlink()` patches the size into the reply and the daemon cache only; the durable inode is created with size 0 and the coherency repair is regular-files-only. Tools that size a `readlink()` buffer from `st_size` record empty targets. Conformance suites miss it because they stat inside the 1 s window | `:13441`, `:11033` | P1 | Commit `size = target.len()` in the create transaction |
| POSIX-4 | Every `readdir` of a non-root directory synthesizes `..` via `find_parent_of_child`, an unindexed full scan of the whole dentry tree of every metadata volume. The function's own doc comment calls it "cold and rare by construction" — true for the `open_by_handle_at` path it was written for, not for readdir. Every `ls`, `find`, `du`, `rsync`, and `tar` walk pays O(total dentries) per directory | `:13745`, `:13826`; `meta_backend/mod.rs:1187`; `kv/backend.rs:1566` | P1 | Store a parent pointer in the inode record, or memoize `child → parent` per readdir session from the LOOKUP that reached the directory. Also remove the `.unwrap_or(1)` fabrication |
| POSIX-5 | `write(2)`, `ftruncate`, `fsync`, and `fallocate` can return `EAGAIN` — `LockFailed` after a 5 s lease timeout maps straight through. POSIX reserves `EAGAIN` for `O_NONBLOCK`. `copy_file_range` already encountered this and received a bounded retry with the fstests provenance recorded; the write path retries only `FencingTokenExpired` | `src/error.rs:61`; `fuse_client.rs:12855`, `:13258`, `:14361`, `:14466`, `:14556` | P1 | Retry with backoff until the watchdog escalates, or map to `EIO` after exhaustion |
| POSIX-6 | Errno derives from substring matching on English error text, so every `InvalidOperation` message is load-bearing wire format. An out-of-space refusal phrased "insufficient capacity" returns `EINVAL` to `write(2)` | `src/error.rs:63-72` | P1 | Structured errno field or split variants; a test pinning each mapping. This is a one-way door once applications depend on it |
| POSIX-7 | Interception mount vs `MAP_SHARED`: shim writes bypass the kernel, invalidation is rate-limited to 1000 ms, and a dirty mapped page writes back over ring-written data (or the reverse). `ipc_host.rs:1730` refuses binding for `O_APPEND`/`O_SYNC`/`O_TMPFILE` but cannot refuse mmap, since the mapping is created after the bind | `src/ipc_service.rs:275`; `src/ipc_host.rs:1730` | P1 | Interpose `mmap`/`mmap64` and poison the binding for any fd receiving a `MAP_SHARED` mapping — the fork-child poison machinery already exists |
| POSIX-8 | `i_size` / `SEEK_END` incoherence after ring writes: the only refresh is the rate-limited whole-inode invalidation, so a `lseek(SEEK_END)` + write append can land at a stale offset | `src/ipc_service.rs:275-307`; `fuse_client.rs:15461` | P1 | Fire an attrs-only invalidation on every size-changing ring write, exempt from the rate limiter, plus one at last unbind |
| POSIX-9 | `readdirplus` silently drops entries whose `getattr` fails, so `readdir` and `readdirplus` can disagree and `rm -rf` can complete its readdir then fail `rmdir` with ENOTEMPTY | `:13854-13863` | P2 | Propagate as a stream error or emit with a minimal attr and 0 TTL |
| POSIX-10 | `copy_file_range` never updates the destination's mtime, and the size-commit error is discarded | `:14249-14262` | P2 | Set mtime; propagate the error |
| POSIX-11 | Directory parent-nlink decrement is silently skipped at `nlink <= 2`, making any deficit permanent; `find`'s leaf optimization then skips real subdirectories | `kv/backend.rs:4626-4636` | P2 | Warn and count when the guard suppresses a decrement |
| POSIX-12 | `fallocate(mode=0)` reserves nothing, so `posix_fallocate` success does not prevent a later ENOSPC | `:14578-14590` | P2 | Reserve, or document as a declared deviation in `docs/operations.md` (the generic/213 adjudication covers reporting, not this contract) |
| POSIX-13 | `fallocate`'s extend path uses a bare fencing-token snapshot; `setattr` was already fixed away from this pattern with the reason recorded, and the punch/zero arm uses the shared lease correctly | `:14584-14589`, cf. `:13244` | P2 | Use `get_or_acquire_lease` |
| POSIX-14 | `fh = inode` for every open, and `open_count` is a bare refcount whose only decrement is a `RELEASE` that **FUSE-2** can lose — so an unlinked-open file whose RELEASE was lost is never reclaimed | `:12255`, `:4522` | P2 | Real file-handle allocation, or reconcile counts at mount |
| POSIX-15 | Rename-overwrite defers the replaced inode's teardown to FORGET; a daemon exit in that window orphans it, and the mount-time reconciliation the comment refers to does not appear to exist | `:13617-13620`, `:14610` | P2 | Add the sweep or correct the comment |
| POSIX-16 | Close-time writeback errors are never reported: `flush` discards the flush result and `release` discards three more, and there is no errseq-equivalent per-inode latch | `:14368`, `:14413-14418` | P2 | A per-inode error latch consumed once by the next `fsync`/`flush` on any fd |
| POSIX-17 | The `noatime` design decision has a wider practical reach than the test adjudication records — Maildir new-mail detection, `tmpwatch --atime`, `updatedb` freshness, HSM agents, and `find -atime` all silently no-op | — | P3 | Name the affected tool classes in `docs/operations.md` |
| POSIX-18 | `symlink()` accepts a 4096-byte target; `PATH_MAX` includes the NUL | `:13410` | P3 | Off-by-one |

Verified correct and requiring no change: `default_permissions(true)` posture, umask handling, setgid inheritance, killpriv-v2 clearing, POSIX-ACL refusal, rename2 flag handling (including `NOREPLACE`/`EXCHANGE`/`WHITEOUT` and the correct refusal of unknown flags), `rmdir` emptiness, open-unlinked file lifetime, timestamp range and `UTIME_*` handling, `EFBIG` boundaries, the fallocate mode matrix, readdir cookie stability, advisory-lock locality, `O_APPEND` refusal under interception, and the size-published-after-data ordering.

---

## 6. Distributed lock manager and multi-node scaling

This section specifies the program to make the lock manager genuinely distributed and to support the stated target of 15,000 concurrent client nodes. It is the output of three independent workstreams — code forensics, a comparative study of production distributed lock designs, and scaling arithmetic against the project's own measured numbers — which converged on the same conclusions.

### 6.1 Current state

`src/dlm.rs` is 345 lines and contains no network, no persistence, no TTL, no renewal, no revocation, no shared mode, and no cross-process visibility.

| Documented (AGENTS.md / README / CLI) | Implemented | Anchor |
|---|---|---|
| "Distributed lock manager" | Process-global `scc::HashMap` | `dlm.rs:77`, `:81` |
| "Local **or distributed** logical volume backend" | `pub enum MetaClient { Local }` — one variant | `:95-98` |
| "Cluster leases on Metadata Volumes" | Leases touch no volume, no metadata, no disk | entire file |
| "TTL + background renewal", "heartbeat renewal" | No TTL on a held lease, no renewal, no heartbeat. The `ttl` argument is the *waiter's* deadline; the error text says so | `:216`, `:250-253` |
| "Fencing tokens: monotonic per-file" | Monotonic per file **per process**, from 0, non-persistent | `:233-238` |
| "local caches must re-validate after lock key loss" | No such event exists; `active_leases` hits return unconditionally | `fuse_client.rs:6929-6934` |
| Shared/read mode, conversion, revocation, deadlock detection, fairness | None. Every lock is exclusive; `notify_waiters()` is a broadcast and barging is unbounded | — |
| `redis_url`, `MetaConnection`, `BoundConnection`, `MockPubSub`, `MockMessageStream`, `MockMessage` | Vestigial, zero callers. `MockMessageStream::next()` sleeps 999,999 s | `:89-116`, `:318-345` |
| `lease_acquire_ok` / `lease_acquire_fail` on the stats inode | **Never incremented anywhere** — permanently 0 **[V]** | `fuse_client.rs:2886`, rendered `:5508` |

The last row has a downstream consequence: the 2026-07-14 baseline demoted the DLM-lease-batching lever citing "`lease_acquire_* = 0` across every metadata storm." That specific evidence is not load-bearing, because the counters cannot be nonzero. The conclusion is independently supportable from the call-site census below, but the cited number should be replaced.

**Two distinct subsystems share the name.** `src/meta_backend/dlm.rs` is a 4096-way stripe array of `tokio::sync::RwLock` serializing the owner's own KV commit apply. It has real shared/exclusive modes and a canonical acquisition order, and it is intra-process **by construction** — a remote node has no node cache to serialize into. These locks can never become remote, and conflating the two is the most likely way to get a redesign wrong.

### 6.2 Scope: smaller than expected in one dimension, larger in another

**Smaller: the cluster lock surface is tiny.** `acquire_lock` has exactly **three** production call sites (`fuse_client.rs:6949`, `routing.rs:10804`, `:10815`). Leases are acquired **once per inode per open-for-write episode** via `get_or_acquire_lease` (`fuse_client.rs:6928`), cached in `active_leases` (`:4147`), and released at last close (`:14438`) or reclaim (`:11223`). Metadata operations — lookup, getattr, create, mkdir, unlink, rename, link, readdir — take no cluster lease at all. Fencing-token *reads* are far more common (~24 sites, several per write/publish/flush) and are the term that would dominate a naive remote design.

The shape is already delegation-like rather than round-trip-like, which is the favorable starting point.

**Larger: single-writer assumptions are baked into durable formats.** Ranked by what gates everything else:

| # | Assumption | Anchor | Why it gates | Fix class |
|---|---|---|---|---|
| 1 | **Block refcounts and the free list have no on-disk representation** — derived by a full-tree walk at mount **[V]** | `block_allocator.rs:98`, `:1011` | Without durable shared ownership accounting, no multi-writer data path is expressible. W1, clone/CoW, reclaim, and fsck C2/C3 all read it | New durable structure. The largest item |
| 2 | One journal ring head per volume | `kv/journal.rs:254`, `:429` | Two committers cannot share one lap-numbered append space; the loser's pages classify as torn and are silently dropped | Per-writer rings + replay merge |
| 3 | One A/B extent bitmap and one `advance_durable` tail | `kv/alloc_ext.rs:11-40`, `:641` | Whole-volume single-appender | Partition bitmap pages per writer |
| 4 | One A/B root ledger, `slot = seq % 32`, newest-valid-wins | `kv/checkpoint.rs:92-136` | Two checkpointers overwrite each other's tree state | Per-writer slot ranges |
| 5 | `next_ino` is a per-mount atomic over a shared namespace | `kv/backend.rs:527`, `:1109` | Duplicate inos alias files immediately; also silently underpins IPC binding identity | Per-writer cursor ranges — **cheapest of the five**; VL5b `slot_cursors` already exist |
| 6 | Block keys are bare reusable device offsets | `routing.rs:5681-5702` | Makes a stale binding structurally undetectable (§6.3) | Key format: `offset ‖ incarnation` |
| 7 | `writer_claim` is singular (Write-Exclusive, one holder) | `kv/backend.rs:230`, `:2508` | Expresses exclusion, not partition membership | Claim-set record + NVMe registrants |
| 8 | `active_block:` / `active_block_ext:` / `mapping:` keys have no writer scope | `lib.rs:361`, `:370`, `:411` | Shared keys naming node-private staging payloads; recovery cannot classify foreign records | Writer-id key component |
| 9 | Layout-delta chains name their base with a process-local token | `routing.rs:172`, incompat bit 5 | Divergent chains fold to divergent layouts | Durable per-ino layout version |
| 10 | Staging generation is the volume-set uuids only | `main.rs:4484` | Identical on every node; designed to catch reformats, not peers | Add node identity to the stamp |

And one runtime item with no on-disk footprint that is arguably harder than any of the ten: **the KV node cache is load-once RAM-authoritative** (`node_cache.rs:1-35`). A node is read from the device exactly once and thereafter served from immutable arc-swapped snapshots, with no revalidation path — because in a single-writer design there cannot be another appender. Two writers on one volume produce divergent trees and mutually destructive checkpoints. The tractable answer is to ensure two writers never cache the same node — **partitioning, not cache coherence.**

### 6.3 Coherence obligations under a second writer

The following are currently correct *only* because there is exactly one writer.

**Block-key binding.** The read path's serve proof is: bytes for key K serve for block *b* iff (a) the fetch was incarnation-valid and (b) the current map still binds *b → K*. Both premises are process-local. (a) reads a per-process seqlock that returns `UNKNOWN_STABLE` for any offset this node did not itself allocate (`incarnation_core.rs:29-33`); (b) is deliberately consulted *without* a TTL gate (`routing.rs:5626-5633`) on the argument that local merges always republish before freeing. So if node A overwrites a block (CoW), frees the offset, and the allocator reissues it to a different file, node B — whose cached map still binds *b → K* and whose incarnation word is untouched — serves the other file's bytes with no error and no counter. On a passthrough volume (the default) this is silent; on a transformed volume the AEAD tag fails loudly, which is the one honest degradation.

**W1 sole-owner patch.** `begin_patch_sole_owner` (`block_allocator.rs:561`) reads a process-local refcount map, and the `SeqCst` fence is a store-buffer fence between two words in one address space. A block cloned on node A has refcount 2 on A and 1 on B, so B's aligned overwrite patches in place and mutates a file it never touched. `patch_ineligible_shared` never increments. W1 is the primary random-write path at 61–67 k IOPS.

**Reclaim and discard.** "Offsets stay non-reallocatable until reclaimed" is a per-process invariant about shared hardware. A frees `O` and queues a `BLKDISCARD`; B independently frees its stale reference, reallocates `O`, and writes; A's reclaimer then discards the range B just wrote.

**Invalidation surface.** `notify_inval_entry`, `notify_delete`, `notify_store`, and `notify_retrieve` all exist in the fork and have **zero callers** — there is no dentry-invalidation surface at all. Only `notify_inval_inode` is used, from three sites, two of which are killpriv side effects, and its result is discarded (`let _ =`). Negative-entry caching is a shipped metadata-throughput lever with an operator-tunable TTL and would poison lookups cluster-wide. `FOPEN_NOFLUSH` is set on every regular open with the justification written in the code: *"Close-to-open visibility across mounts is moot under the M1 single-writer guard."*

Full obligation table (cache → trigger needed → mechanism available today): the daemon `metadata_cache` (dirty entries are never revalidated at any age), `attr_cache`, `dir_entry_cache_v3` (300 s TTL keyed on a process-local generation), the hot-block tier, the read-lane hold, the NVMe read cache, the GDS cache, path-keyed LRU entries (outside `purge_block_key` by construction), the `killpriv_clean` latch, the placement table, the KV node cache, block refcounts, incarnation words, the free list, elided discard debt, and the shim's fd bindings. Of these, only the five block-key stores have a working purge primitive — see §8.

### 6.4 A read-only second mount is not currently possible **[V]**

`KvMetaBackend::open` takes `flock(LOCK_EX | LOCK_NB)` **unconditionally, before any read/write classification** (`kv/backend.rs:765`), with a refusal message reading "concurrent mounts of one metadata volume are refused (single-writer guard)." The `read_only` field derives solely from `sb.unknown_ro() != 0` (`:1133`) — a forward-compatibility degradation for unknown RO feature bits, not a mount option. There is no `-o ro`, no `--read-only`, and no env knob. Cross-host, a would-be reader is refused `FreshForeign` by the D0 gate.

The shipped model is one mount per volume set.

### 6.5 Scaling budget

Derived from the project's measured numbers.

**Per-volume metadata ceiling.** The conveyor pass is a serialized ~0.78 ms server at ρ ≈ 0.92 (`2026-08-01-rewrite-publish-drain.md` §3) ⇒ ~1,180 passes/s; at the measured group size of 2–3 that is **~2.5–3.5 k tx/s per metadata volume**, corroborated by the field capture's ~2,600 block-publishes/s against a 21–25 k device-writes/s namespace ceiling.

**Lock-operation rate is bounded by the metadata plane, not by client count.** Because leases are per-open, the cluster aggregate is 0.3–1.8 M lock ops/s whether there are 100 clients or 15,000. Sharded 64 ways that is 5–28 k ops/s per shard — comfortably servable. **This is the central positive result: volume is not the constraint.**

**What is the constraint, in order:**

1. **Latency.** An uncontended acquire sits inside a 64 µs under-lock span that 86.9 % of creates already fit into. Adding one 250 µs fabric RTT takes the create wall from 110 µs to 360 µs — **9,090 → 2,778 creates/s, a 69 % regression** that would undo the metadata-throughput program in one round trip. **Requirement: ≥ 99.5 % of lock operations must be served from a locally cached or delegated token.** The `active_leases` cache already achieves this in steady state; only the miss path and the revoke path are missing.
2. **Exclusive-only locking caps shared directories.** With one exclusive parent lock at a 360 µs hold time, the *entire cluster* performs ~2,778 creates/s — 3.3× slower than one client today — and 15,000 queued waiters exceed `acquire_lock`'s hard-coded 5 s budget, producing deterministic `EAGAIN`. The remedy already exists locally: shared parent lock plus Δtime merge records (`design-cow-kv-metadata` §4.4 pt 6). It needs to be exported, not invented.
3. **The membership plane saturates at ~4,550 clients.** Each client writes `client:{uuid}` every 10 s as a full journal transaction under an **exclusive `I{1}` guard**, and ino 1 routes to slot 0 → one volume unconditionally. At the measured saturated `commit_tx_wait` of 2.198 ms that volume serializes 455 beats/s against the 1,500/s that 15,000 clients require. Adding volumes does not help — these are xattrs on a pinned inode. Past saturation, records age past the 45 s TTL and every liveness consumer begins treating live mounts as dead. The read side is worse: `mount_registrations()` is `listxattr(1)` plus one `getxattr` per client, each taking a shared `I{1}` lock, per `squeezefs clients` / `status` / `format` preflight.
4. **Revocation fan-out.** A 15,000-holder revoke is 30–75 ms of serialized daemon time on a single un-batched notify lane with no per-destination queue. Whole-object callbacks on directories would be ~42 M/s — structurally impossible. Bit-split capabilities take that to ~0 in steady state.
5. **Memory.** `FENCING_MAP` has no removal path **[V]** — ~105 B per distinct inode ever write-locked, R5-invisible, ~10.5 GB at the stated 100 M-inode cap. Server-side, GPFS's published constants put 15,000 clients × 4,000 cached locks at 27.8 GB and imply ≥ 51 lock authorities.

### 6.6 Reference designs and what applies here

Every system that scales past ~1,000 clients makes the uncontended case touch no network; they differ in what the client caches and how it is reclaimed.

- **Lustre LDLM** (50 k+ clients in production). Applicable: **intent locks** — the lock rides the metadata RPC (`IT_OPEN`, `IT_CREAT`), so acquisition is never a separate round trip; **inodebits** — LOOKUP/UPDATE/PERM/LAYOUT as separate bits on one resource, so a create does not conflict with a sibling create; CR/CW modes; LVB/glimpse (read a file's size without revoking the writer); server-driven client LRU shrink (SLV/CLV). Avoid: a single global callback timeout, and eviction as the only response to a slow client.
- **GPFS.** Token authority **sharded by inode number across manager nodes** — the project already has the equivalent map. Also: required-vs-desired byte-range requests, and the **metanode** pattern (elect a per-file metadata owner and ship deltas to it rather than revoking), which is exactly the existing Δtime merge one level up.
- **Ceph MDS capabilities.** The finest-grained model: bit-split caps, the issued/wanted/implemented triple, over-issue on grant, and **rate-limited bulk recall** (`mds_recall_max_caps`) — the mechanism most clearly absent here.
- **NFSv4.1.** The client-initiated backchannel (traverses NAT, and its health is observable) and the rule that a client with no healthy callback path receives no new delegations.
- **DAOS.** Avoids a lock manager via HLC epochs and optimistic conflict-at-commit. The epoch idea is applicable — the fencing token is already a per-object generation. The "no client tracking" property is not, because a POSIX FUSE mount has a kernel page cache and therefore requires callbacks.
- **Classical DLM directory nodes**, and the FAST '26 *Lockify* result, which is directly on point: on NVMe-over-TCP shared storage, creation throughput degrades up to 86 % as client count grows, with lock operations at 47 % of creation latency, because a new object's directory node is usually remote. Their remedy — the creating node self-designates as owner and notifies the directory asynchronously — is **free here**, because inos are monotonic and never reused.

**Mandatory at 15,000 clients:** client-side lock caching, intent locks, shared/concurrent modes or capability bits, server→client revocation, callback rate limiting and per-client aggregation, a bounded client lock cache with LRU cancellation, server-driven cache shrink, sharded lock authority (≥ 16–64), sharded membership records, and per-client fencing.
**Optional:** byte-range tokens, the metanode pattern (strongly indicated but not a correctness requirement), LVB/glimpse.
**Explicitly not:** a full nine-mode matrix, a separate lock-server tier, or consensus on the operation path.

### 6.7 Recommended architecture

> **Slot-homed lock service; client-cached tokens; function-shipped metadata; delegated data plane; epoch-fenced.**

Four structural decisions:

1. **Metadata authority is ownable, not lockable.** The KV engine is RAM-authoritative and single-writer by construction, so metadata mutations are **function-shipped to the volume's owner**, not lock-shipped. The 4a `DlmGuard`s stay exactly where they are — a remote node has nothing to serialize into, and inserting a round trip inside a server already at ρ ≈ 0.92 would multiply through the queueing formula.
2. **Lock homing rides the existing slot map.** `slot = (ino − 2) % 65536` is a durable, **already-online-migratable** homing function with a per-slot cutover gate that parks operations before 4a acquisition — a lock directory plus a remastering protocol, already built, tested, and stamped on disk under incompat bit 6. No hash ring needs to be invented, and the lock master and the metadata authority are the same process, so a metadata RPC and its lock are one round trip.
3. **The uncontended acquire costs zero network operations** — four ways: the node owns the slot (today's code path unchanged), the token is cached (`active_leases` already), the object is covered by a subtree delegation, or the grant piggybacks on the metadata RPC the operation was going to issue anyway. There is no operation that needs a token but performs no metadata RPC on the object first.
4. **Fencing becomes globally sound with one durable word and one atomic.** `token = (term << 40) | grant_seq`, where `term` is a new durable field on the `writer_claim` record (incompat bit 7, committed and barriered before arming — the gate already does this unconditionally) and `grant_seq` is a **single** per-owner `AtomicU64`. Global monotonicity implies per-object monotonicity; every existing comparison (`<`, `==`, `.max()`) remains monotone-safe; and `FENCING_MAP` is deleted outright, closing the unbounded-growth item as a side effect.

**Lock modes.** Four modes plus capability bits (NL / CR / CW / EX with LOOKUP, UPDATE, PERM, LAYOUT, XATTR, DATA bits) covers every shape in this filesystem. Two consequences to pin in code: the W1 patch predicate requires whole-inode exclusive custody, so range-shared custody needs a seventh clause in the existing decision ledger (`patch_ineligible_range_shared`) to keep predicate rot visible; and CW should ship disabled until a verb issues it, per the no-dead-code rule.

**Transport.** Build one `cluster_wire` rather than extending `job_wire`. `job_wire` is the right shape (length-prefixed, schema-versioned, TLS-capable, leases with TTL and heartbeat, fencing-checked proposals) and the wrong implementation for a custody-bearing protocol — see **VAL-6** for the specific gaps, plus `serde_json` framing at roughly 1 µs/frame against a 10 µs budget. Then port `job_wire` onto it, which closes **VAL-6** as a side effect and leaves one cluster transport in the tree. Owner-side RPC handling runs on pinned service threads (the `ipc_service.rs` pattern), never on the conveyor's task.

**On external consensus.** Every arbitration decision reduces to one question — who owns this volume — and NVMe Persistent Reservations answer it more strongly than a consensus service would, because a Write-Exclusive reservation does not merely decide the winner, it enforces the decision by rejecting the loser's writes. The honest exception is non-PR substrates, where cross-host arbitration is detection-grade only; the recommendation there is a pluggable `OwnershipArbiter` trait with a PR default, never a hard dependency — and **multi-writer should refuse to arm on non-PR substrates**, which includes most developer boxes and the repo's own loop substrate.

**Recovery.** Client failure: the owner's TTL fires, the client epoch is marked dead, its grant bucket is dropped (O(1)), and blocks allocated under that epoch enter a do-not-reallocate quarantine until the epoch is proven drained — the job wire's fresh-destination law applied verbatim. Owner failure: lock state is RAM-only and is reconstructed by re-assertion (NFSv4 style) — the D0 ladder elects the successor, the successor bumps `term` durably before arming (which makes every old-term token stale by construction), and opens a grace window accepting only reclaim requests, refusing conflicting fresh acquires. Without that window, failover triggers a cluster-wide forced-flush storm at the worst possible moment.

**Two lease clocks, and the client's is stricter.** `T_self = T_owner − 2·skew_max − D_purge`, both on monotonic clocks anchored on the RPC round trip. A client that cannot renew by `T_self` fail-stops the affected objects *itself* before the owner can grant them elsewhere. False-positive eviction then costs availability and at worst unpublished dirty data, never divergence.

### 6.8 The increment to target first

**"One writer plus N coherent readers" requires no distributed lock manager.** Readers take no leases. It requires six things, in dependency order:

1. A real read-only mount mode — `flock(LOCK_SH)`, a bypass of the `FreshForeign` refusal for RO, and the write gate extended past metadata to cover the block allocator, the reclaim queue, W1 and in-place overwrite, and `recover_active_blocks_v3`'s free-completing arm.
2. **A metadata revalidation path for the node cache** — the hard item, but much easier for a reader than a writer (no dirty nodes, no SMOs, no pinned interior state to preserve). Cheapest credible design: poll the A/B root ledger (a 4 KiB read) at a bounded cadence and drop every cached node not covered by the new roots. Readers then lag by one checkpoint interval, which is an honest and documentable consistency model.
3. **A freed-offset grace period** — the highest-value single item in the coherence analysis, because it converts §6.3's cross-file staleness into bounded staleness. The writer already maintains the freed-offset log (the reclaim queue); refuse to reallocate an offset until every registered reader has acknowledged passing that epoch, riding the existing `client:` heartbeat. A reader that fails to acknowledge is fenced, not waited on.
4. Kernel TTLs set to the checkpoint interval; `dir_entry_cache_v3`'s 300 s TTL cut to match; writeback cache off for readers.
5. Purge on revalidation — where the codebase is best prepared: `purge_block_key` is one call covering all five block-key stores, with a grep-guard test preventing a sixth from being forgotten. **The invalidation primitive already exists and is complete; only the remote trigger is missing.**
6. Reader-side data-plane lockdown (mostly deletions behind the RO flag).

Items 1, 4, 5, 6 are days of work; 2 and 3 are weeks.

### 6.9 Staging plan

| Stage | Ships | Guarantee after | Gate |
|---|---|---|---|
| **S0** | `LockManager` trait + `LocalLockManager` (byte-identical semantics); delete `MetaClient`, `MockPubSub`, `MockMessageStream`, `MockMessage`, `MetaConnection`, `BoundConnection`, `redis_url` | unchanged | mdstorm + rand-4k within noise; `dlm_acquire_time` unchanged |
| **S1** | Single global `grant_seq` replaces per-object `FENCING_MAP` | unchanged, plus the unbounded-growth item closed | RSS flat across a 10⁷-inode walk |
| **S2** | `WriterClaim.term`, durable, barriered pre-arm; composed tokens (incompat bit 7) | **fencing is remount-monotone** | red-first crash/remount/stale-record test |
| **S3** | `cluster_wire`; `job_wire` ported onto it | unchanged, plus **VAL-6** closed | job-wire fidelity legs unchanged |
| **S4** | Slot lock manager, **solo mode only** (one node owns every slot) | unchanged; every path asserts `dlm_rpcs == 0` | **the gate: mdstorm, rand-4k, and the scoreboard all within noise** |
| **S5** | **Read-only coherent client mounts** (§6.8) | **1 writer + N readers**; long TTLs under a read token | new capability row: N readers × cached stat/s |
| **S6** | Membership off the journal (lease-based liveness) | same, heartbeat tax removed | volume-0 journal tx/s → ~0 at 15 k |
| **S7** | Data-plane custody-epoch fence + dead-epoch allocation quarantine + WERO on data namespaces | the fenced-DMA gap closed | a stop/resume-past-TTL leg must show device rejection |
| **S8** | Metadata function shipping (the 12 `Metadata` verbs on the wire, pipelined) | **multi-writer metadata**; single-threaded latency regression measured and published | serial `tar -x` A/B, published even if it regresses |
| **S9** | Multi-writer data plane (custody tokens, remote clients DMA directly) | **full multi-writer on PR substrates**; refused on non-PR | 15 k-shaped write fan-out with write-amplification columns |
| **S10** | Subtree delegation + client-owned-slot placement | serial-latency regression recovered | `tar -x` back to the S0 baseline |
| **S11** | Byte-range custody + the W1 seventh ineligibility clause | shared-file parallel write | MPI-IO-shaped row |

Existing single-writer deployments ride S0–S7 with **no behavior change**: the mount reports `dlm_mode=solo`, `dlm_rpcs=0`, and every acquire is the same `scc` probe it is today. Multi-writer is opt-in per mount, refuses on non-PR substrates, and refuses if the format lacks the incompat bit.

**Loom models required:** `grant_table_core`, `token_cache_core`, `lease_clock_core` — each `#[path]`-included against the shipped file with weakening evidence, per the existing precedent.

**New counters (row-validity gates):** `dlm_mode`, `dlm_term`, `dlm_rpcs{acquire,revoke,renew,meta}`, `dlm_token_cache_{hits,misses,evictions}`, `dlm_delegation_{grants,hits}`, `dlm_revokes{issued,acked,timed_out}`, `dlm_revoke_phase_ns`, `dlm_thrash_demotions`, `dlm_grace_{reclaims,conflicts}` (conflicts must stay 0), `dlm_grant_table_bytes` and `dlm_token_cache_bytes` (both R5), `dma_fence_drops` (must stay 0), `dlm_quarantined_offsets`.

### 6.10 Ranked risks

| # | Risk | Resolving evidence |
|---|---|---|
| R1 | Function-shipped metadata degrades single-threaded latency. At 50–150 µs RTT a serial operation stream drops from 9,100/s to 6.7–20 k/s before owner queueing, and `tar -x`, `make`, and `rsync` are all serial streams | Measure the actual RTT on the target fabric with the S3 wire before S8 lands; then measure S10's delegation recovery. If delegation cannot recover it, the honest product statement is "remote clients are throughput-oriented; latency-sensitive metadata work runs on the owner" |
| R2 | The data-plane fence gap (§7, RES-6) is a correctness blocker the moment a second writer exists | The S7 stop/resume-past-TTL leg on a PR-capable substrate, both stacks; must show device rejection, not only a latch |
| R3 | Revoke drain p99 exceeds the revoke deadline because the publish and staged-flush step is unbounded under writeback backpressure — the same class that produced the earlier transport-lease stall | `write_pipeline_phase_ns` and `publish_phase_ns` already exist; derive the deadline from their live p99 (never a constant) and pin with a stall seam, as `transport_lease_overlong_tests.rs` does |
| R4 | Ownership granularity is the volume, so the modeled load needs ~46 metadata volumes, each with its own claim, PR registration, checkpoint task, journal ring, and node cache | A 46-volume mount's aggregate RSS, checkpoint CPU, and `meta_kv_journal_entries_per_volume` balance |
| R5 | Revoke storms on hot shared objects at 15 k readers | `dlm_thrash_demotions` under a synthetic hot-object storm; verify the demotion valve engages before the fan-out hurts |
| R6 | Owner failover at scale — 1,875 clients × 1 k tokens reclaiming inside a 45 s grace window | Batch reclaim (one frame per client carrying its whole token set); measure grace-window completion with a killed owner and 1,875 simulated clients |
| R7 | Non-PR substrates — including most developer boxes and the loop substrate — are exactly where multi-writer must refuse. Every multi-writer test therefore needs the tcp substrate or the fidelity rigs | Confirm PR support on `SQZ_DEVSUB_TRANSPORT=tcp` |
| R8 | Cross-volume rename is already non-atomic (and see **DUR-7**); multi-writer makes the window observable | Not a regression; must appear in the guarantee table |
| R9 | Writable shared `mmap` across nodes is not supported (§6.3, POSIX-7) | Guarantee table plus a refusal, rather than silent incoherence |

### 6.11 An item to fix now, independent of the program — **P1** **[A]**

Fencing tokens are in-RAM only and restart at 0 in every process. A staged or extent record written before a crash carries token 5; the fresh mount's counter reads 0. The recovery check is `rec.fencing_token < current` (`fuse_client.rs:7333-7347`), so `5 < 0` is false and the stale pre-crash record is adopted as current — `extent_records_stale_discarded` is therefore structurally near zero, and the documented "remount law: stale fencing tokens discard staged work" **cannot fire at mount time**. What actually protects this path today is the staging generation stamp, not the token.

Same class, different reach: offline `fsck`, `defrag`, `clone`, and `config` each construct their own `DlmClient` in a separate process with an empty map, so `get_fencing_token_ino` returns 0 and every `save_metadata_to_backend` fence check evaluates `0 < 0` — structurally vacuous. Contained today only because those verbs take the D0 guard.

**Acceptance.** A red-first test: stage an extent record, force-crash, remount, assert the record is discarded. If it reproduces, S2 should be scheduled independently of the rest of the program.

### 6.12 Documentation requirement — **P1**

Decide whether "15,000+ concurrent nodes" means 15,000 writers or 15,000 clients of which a minority write; the design serves both but the deployment recipe differs by an order of magnitude. Correct AGENTS.md and README to describe the shipped product — currently `AGENTS.md:88`, `README.md:5`, `README.md:15`, `AGENTS.md:106-112`, `AGENTS.md:659`, `main.rs:20`, and `docs/reference-clients-survey.md:103` all describe capabilities that are not implemented, and several code comments depend on the stronger claim being true (notably the `FOPEN_NOFLUSH` justification at `fuse_client.rs:4016`).

---

## 7. Resource management, concurrency, and lifecycle

**Positive result worth recording:** the `await_holding_lock` class is **clean** across all 142 k never-linted lines. A systematic multiline scan plus hand inspection of every `if let`/`match`/`while let` scrutinee containing a lock call found zero true positives — the `let x = { guard; … };` convention is applied consistently, including in the two places most likely to get it wrong (`sync_coalescer.rs`, `conveyor_core.rs`), both of which document the invariant beside the code. Lock ordering was traced through the write, read, create/unlink, rename, truncate, fsync, and IPC sync paths with no inversion found; `inode_pair_lock_order` correctly orders by shard index rather than inode number; and KV node-lock discipline holds across fifteen acquisition sites with zero device I/O under a lock.

| ID | Item | Anchor | Priority | Required behavior |
|---|---|---|---|---|
| RES-1 | `INODE_META_LOCKS` (level 3.5) held across `free_block`, which parks up to `SQUEEZEFS_RECLAIM_CAP_PARK_MS` (default 1000) **per key, in a loop**. 64 displaced keys under reclaim pressure holds a 4096-way stripe for up to 64 seconds | `routing.rs:6781`+`:6816`, `:6604`+`:6621`; `block_reclaim.rs:658` | P1 | Collect keys under the lock, free after the guard drops — the pattern `truncate_layout` (`:6063`) and `write_striped` (`:8886`) already use |
| RES-2 | `FENCING_MAP` has no removal path — one entry per distinct inode ever write-locked, ~105 B each, R5-invisible. `LOCK_MAP` is correct by contrast (nonce-conditional removal) | `dlm.rs:81`, `:233` | P1 | Fold the generation into the lease object (deleted outright by DLM S1), or bound with an LRU and register with R5 |
| RES-3 | `ReadLaneHold.fifo` pushes one `(u64, String)` per deposit and only `trim_to` pops, gated on `bytes > target`. Coverage retirement removes the map entry and the byte gauge but not the FIFO node, so it grows monotonically in the **healthy** steady state and the R5 gauge is structurally blind to it | `read_lane.rs:308`, `:380`, `:398` | P1 | Pop-and-discard tombstones on retire, or bound the FIFO length |
| RES-4 | Eviction-channel bytes: the bound exists (256 MiB) and the gauge exists and is commented as "the R5 gauge", but no `Component::new` registration. Three `LruCache` instances ⇒ up to 768 MiB of live payload outside the budget | `cache/lru.rs:34`, `:137`; registrations at `fuse_client.rs:11529-11737` | P1 | Register two components with shed closures. Three-line fix |
| RES-5 | `IpcHost.threads` and `JobWireHost.handles` are push-only — one `JoinHandle` retained per connection ever accepted, both reachable before authentication | `ipc_host.rs:1264`; `job_wire.rs:855` | P1 | `retain(|h| !h.is_finished())`; see **VAL-5d** and **VAL-6** |
| RES-6 | A fenced daemon can still issue data-plane DMA. Reservations cover metadata volumes only; `nvme_dev.rs` has no fence predicate. The reclaim paths are correctly fenced; the write pipeline is not (`write_pipeline_fence_drops` is DLM-token classification, not the D0 latch) | `job_wire.rs:1582`; `write_pipeline.rs:196`; `nvme_dev.rs` | P1 | Gate DMA submission on the D0 `failed` latch (one relaxed load per submit); take a data-namespace reservation for the mount lifetime; document the guarantee class |
| RES-7 | `run_job` has no panic guard and its `JoinHandle` is never observed, so a panic silently removes a worker, leaves `claimed = true` forever, and — by the mover-serialization pin — permanently blocks every mover job on that volume scope | `jobs.rs:1381`, `:978`, `:1298-1319` | P1 | The `PassSentinel`/`PassGuard` shape already in the tree; clear `claimed` on unwind; add `job_worker_panics` |
| RES-8 | Detached `tpc_spawn` upload on the write ACK path: a panic loses the block's write-back with no counter, and the phase histogram under-reports rather than showing the failure | `fuse_client.rs:8401` | P1 | A `tpc_spawn` variant that catches the unwind and counts — closes the class across ~15 data-path sites |
| RES-9 | `write_striped` cancellation detaches per-block tasks that have already allocated and published, leaking blocks recoverable only by fsck | `routing.rs:8731`, `:8843` | P1 | Own the tasks; see **MEM-2** |
| RES-10 | `MemoryCacheShard::remove` leaves the eviction-queue tombstone; `try_evict`'s `max_loops` can then exhaust without freeing, so the shard exceeds `max_bytes` and the **R5 Red clamp can fail to clamp** | `tiering/memory.rs:293`, `:230`, `:200` | P2 | Drain the tombstone on remove; revisit `max_loops` |
| RES-11 | `MEM_BUDGET.tick()` runs procfs reads and a full jemalloc all-arena purge on a tokio worker at 1 Hz — tens of milliseconds of uninterruptible work once per second, precisely under memory pressure | `mem_budget.rs:841`, `:820` | P2 | `spawn_blocking` or a dedicated thread |
| RES-12 | `Drop for UringWorker` and `Drop for DataPlaneSink` join OS threads; dropped from an async context, a tokio worker blocks for the full drain | `nvme_dev.rs:196`; `ipc_service.rs:341` | P2 | Hop through `spawn_blocking` at the teardown sites |
| RES-13 | `dir_gen`, `killpriv_clean`, `Invalidator.last_write`, and `block_allocator.incarnations` all grow without a removal path; the first two are not swept on `forget`/`batch_forget` | `fuse_client.rs:6357`, `:6479`, `:14597`; `ipc_service.rs:272`; `block_allocator.rs:166` | P2 | Add to the forget sweep; bound or budget the others and record the ceiling |
| RES-14 | `ExtCore::claim` spins forever on invariant drift — no cap, no yield, no diagnostic — inside a sync function called from async, permanently consuming a worker | `alloc_ext_core.rs:310-329` | P2 | Bound the retries and fail loud |
| RES-15 | `allocate_block`'s ENOSPC valve loop has no attempt or deadline bound; concurrent reclaim traffic keeps `pending` true so the honest-refusal exit is never reached | `block_allocator.rs:669-683` | P2 | Bound and escalate to `StorageFull` |
| RES-16 | `job_wire`'s fence mutex is held across a blocking NVMe reservation fan-out, and `RouterShardDevice::block_on` uses `block_in_place` per block inside an async loop | `job_wire.rs:1339`, `:385`, `:1503` | P2 | Scope the mutex; batch the block operations |
| RES-17 | `tiering/nvme`'s `active_keys: VecDeque` removes with `retain`, an O(n) scan plus shift per removal under the staging shard write lock | `tiering/nvme.rs:237`, `:578`, `:837` | P2 | Index-based removal |
| RES-18 | fuse3 handler lanes use `unbounded_channel` plus uncapped `spawn_local`; bounded only incidentally by ring geometry from a different subsystem, and the classical sideband is not ring-bounded | `crates/fuse3/src/raw/session.rs:4668`, `:4729` | P2 | A geometry-derived bound, or one sentence of recorded reasoning |
| RES-19 | `free_forensics_tape` is an insert-only global map of full backtrace strings under a global mutex, on the write path when its env knob is set | `block_allocator.rs:11`, `:888` | P3 | Ring buffer of the last N |
| RES-20 | `queue_reclaim_inode` spawns one task per FORGET onto the current lane's `LocalSet`; a `drop_caches` storm piles tasks on one thread | `fuse_client.rs:4578-4589` | P3 | Batch |
| RES-21 | The AGENTS.md lock-order table omits **level 3.5** (`INODE_META_LOCKS`), which `stripe_locks.rs:15` documents and 20+ sites depend on | `AGENTS.md` vs `stripe_locks.rs:15` | P3 | Add it |
| RES-22 | ~20 `debug_assert!` sites assert runtime concurrency outcomes rather than pure arithmetic — the class that previously produced a lost-reply stall. Also ~20 `.expect("… mutex never poisons")` sites whose premise fails if anything under the lock ever panics | `fuse_client.rs:8455`, `:7834`, `:7902`, `:9231`; `cache/active_block.rs`; `slot_core.rs:194`; `ipc_host.rs` (20 sites) | P2 | Convert concurrency-outcome assertions to loud-never-fatal counters; prefer `parking_lot` or explicit `PoisonError` handling |

**Cancellation safety.** Post-M4 there are no per-op timeout wrappers on FUSE handlers and nothing cancels a dispatched handler task, so handler futures are effectively never dropped mid-flight and the unsafe shapes above are latent rather than live from the FUSE surface. That is precisely why **MEM-1** matters: the 30 s timeout in `nvme_dev.rs` is one of the very few places that *does* drop a future mid-operation on the data path, and it does so around a raw pointer. Verified cancel-safe: the commit conveyor (detached by design, guards co-owned to terminal outcome, `Weak`-upgrade failure fails out rather than stranding), the publish conveyor, `SyncCoalescer`, the single-flight read guard, and the IPC serve paths.

---

## 8. Invariants to preserve

These were examined specifically for defects and none were found. Any refactor touching them should treat the existing behavior as the specification.

| Component | Property |
|---|---|
| `src/nt_copy.rs` | Scalar head to 16 B destination alignment, streaming stores only on the aligned destination, unaligned loads for the arbitrary source, scalar tail, unconditional trailing `sfence`. Fence placement is correct for both claimed publication edges |
| `lease_core.rs`, `cqe_core.rs`, `wake_core.rs`, `placed_core.rs` | Store-buffer/Dekker analyses are correct; explicit `SeqCst` *fences* (not accesses) are the right primitive; publish-then-recheck closes the missed-wake races. Each is loom-checked against the **shipped** file via `#[path]` with weakening evidence |
| `SyncCoalescer` | Registration atomic with the `flushing` flag; fan-out and reset share a critical section with a would-be registrant's push (no lost wakeup); the timeout drains both batch and queue; the bound lives *inside* the coalescer so an outer `timeout()` cannot strand it. **DUR-3** is a consumer misusing correct machinery |
| D0 mount gate | Disciplined evidence classification; fresh foreign claims refuse on every substrate; non-PR stale claims never auto-taken; the dead-pid proof requires boot-id match **and** ESRCH **and** flock; the claim commit is barriered before the checkpoint task exists |
| KV journal and replay | Chain-primary walk with `seq == pos` identity, lengths validated before any byte they govern is read, whole-entry checksums, lap-validated resync, drop-confirmation only on a later success. No passing all-zero or truncated unit could be constructed in any of the five checksummed formats. `#[must_use]` `Admission` plus release-on-unwind `PassSentinel` close the budget-leak and wedge classes on every traced panic path |
| Ring admission | `claimed + len + reserve > reusable + capacity ⇒ refuse`, with the caller parking while holding no node locks. A burst cannot outrun wraparound; the only way `reusable_upto` can be wrong is **DUR-3** |
| SMO ordering | Successors barriered before any pointer record can exist; reservation inside the parent-then-child window; entry bytes after release; publish → route-flip → retire with the reader-window rationale recorded |
| `serve_validated` and the IPC descriptor discipline | Snapshot-once linearization, `checked_add` arena bounds, daemon-owned binding table resting on the monotonic never-reused ino law, per-op direction rights. Examined specifically for validate-then-reread; none found. `SlotCore` and `MpscRingView` are memory-safe under arbitrary corruption of shared memory, by construction, loom-checked |
| memfd sealing order | `F_SEAL_SEAL` applied last, so a client cannot add `F_SEAL_WRITE` and lock the daemon out |
| R-6 unified purge | All five block-key stores in one call, traced into every mutation path, with a grep-guard test mechanically preventing a sixth from being forgotten. This is the invalidation primitive §6 needs |
| `TpcScheduler::dispatch` | Dead-lane detection, re-dispatch across all lanes, a counter, and `process::abort()` rather than blackholing a request. The model **FUSE-2** should be rebuilt to match |
| Payload lease machinery | `PayloadArena` owning the buffers and a `dup(2)` of the wake fd; the `CommitGate` park/unpark re-arm gate; the in-place-reply pointer proof; `SendBufs::drop`'s deliberate `mem::forget` for in-flight sends |
| Transport geometry law | `max_pages = ceil(max_write/page)` clamped by `fs.fuse.max_pages_limit`, making `ring->max_payload_sz == payload_sz` structural, with a degrade ladder that never regresses below the pre-L1 posture |
| INIT negotiation discipline | Every non-advertised capability carries a measured or test-anchored rationale and a pinning test. **FUSE-1** is an omission in the reply header, not in the flag selection |
| killpriv-v2 | Matches Linux exactly including the trap (S_ISGID only if S_IXGRP), privs-before-write ordering, the O_TRUNC fold, and latch invalidation on both chmod and `security.capability` setxattr |
| `PARALLEL_DIROPS` + shared parent locks + Δtime merge records | Preserves both concurrency and time correctness on same-directory storms, and is the local half of the mechanism §6 needs |
| `inode_pair_lock_order` | Orders by shard index, not inode number — the correct total order over lock instances — paired with a pointer-equality self-check |
| W1 clone/patch fence | `begin_patch_sole_owner` / `pin_block_validated` is a correct two-word protocol with the fence placed before the word lookup; the clone side handles the unstable case with a bounded retry |
| Coverage-union completion | `record_write` is genuinely order-blind; split and reordered kernel writes are handled; completion fires exactly once |
| The one-merge discipline | `merge_block_mappings_if_epoch` as a single choke point, with "free only what the current map displaced, never the caller's snapshot" correctly implemented |
| `begin_free` → reclaim → `finish_free` | Pop-ownership and reserve-before-pop make concurrent drains sound; the fence latch stops a fenced holder issuing destructive discards |
| Staging generation and format fences | Generation gate before any segment is mapped; dual-marker mid-rebind adoption; future staging formats refuse the mount as a unit |
| fsck repair | Dry-run genuinely default; every class re-verifies against a fresh census before acting; quarantine manifests written and fdatasync'd before the commit mutation; no fabrication where redundancy does not exist |
| `default_permissions(true)` | Load-bearing. Without it every xattr item in **VAL-2** becomes universally reachable rather than mode-gated |
| `mac_eq` | Constant-time MAC comparison |
| Subprocess discipline | All 63 `Command::new` sites are argv-form with no shell |
| SPDK commit pin | `rev-parse HEAD` verified against a constant, tree removed and refused on mismatch; run-dir 0700 and RPC socket 0600 are the pattern **VAL-4** and **VAL-7b** should copy |
| SPDK JSON-RPC client | Connect/call/slow-call timeouts, deadline-driven per-read timeout, incremental parse, and a request-id echo check |
| `preload-release` compile guard | `#[cfg(panic = "abort")] compile_error!` makes a wrong-profile build unrepresentable, and the gate proves the refusal |
| `build.rs` | Honors `SOURCE_DATE_EPOCH`, never fails the build, correct `rerun-if-changed` guarded by `Path::exists()`, no network, no codegen |
| Shim internals | Lazy real-call in `interposed!` (the eager form double-applied every bound-fd write); the TLS reentrancy guard; the `shutdown(2)` in-process vs `close(2)` in the atfork child split; refcounted bindings that refuse to resurrect a zero count; `close_range` segment skipping; `unbind_ino` using CAS not swap; the `O_PATH`-before-access-mode check; `dlvsym` version-exact libaio resolution; LFS-64 alias coverage |

---

## 9. Performance work items

Ranked by expected magnitude × confidence. Each names the instrument that proves it. Fabric-sensitive rows run on `SQZ_DEVSUB_TRANSPORT=tcp` with A-B-B-A ordering and a sustained ≥ 60 s leg.

| ID | Item | Anchor | Expected | Instrument |
|---|---|---|---|---|
| PERF-1 | Land the zcrx read lane through **Z3** gather fusion. Counted upper bound already measured: −65…−68 % RX CPU at 49.5 GB/s, DRAM 0.69 → 0.035 B/B. **Z2 as shipped is pass-count-neutral — do not adjudicate the lane on Z2 rows.** Close **MEM-3** first | `src/zcrx_lane/` | +3–5 GB/s kernel reads | `gather ≡ fill`, per-queue `rx*_bytes ≡ lane bytes`, `zcrx_frame_violations`/`zcrx_lane_poisoned == 0` |
| PERF-2 | Remove the process-global `over_uring` mutex from the request path — taken **4× per READ** on one cache line, shared by every connection clone. The pool is installed once and only taken at teardown, so a per-connection `OnceLock` with liveness from the existing `ready`/`active` atomics is semantically identical and lock-free. The "this one is uncontended" comment predates the current op rate | `tokio.rs:309`, `:320`, `:544`, `:727` | ~4 M lock RMWs/s removed at 1 M IOPS | `perf c2c` HITM; `read_transport_phase_ns.{queue_wait,reply_commit}` |
| PERF-3 | Shard the per-op global counters and phase histograms — ~15 process-global atomic RMWs per transport request, 6+ per IPC serve. `Align64` prevents false sharing but not true sharing, and tight latency distributions concentrate nearly every op in one histogram bucket | `read_phase.rs:93`; `ipc_host.rs:456` | 3–8 % on warm rand-4k | `perf c2c` before/after |
| PERF-4 | Fix the read-fill issue economy: 3.0 ms of the 7.77 ms fill RTT is client-side issue and wake. One thread per device, `IoUring::new(1024)` with no `SINGLE_ISSUER`/`DEFER_TASKRUN`, `submit_and_wait(1)` per pass, a `oneshot` per request — and **`register_buffers` is never called anywhere in the tree**, so every DMA re-pins user pages. Re-measure first; the NUMA fix may have trimmed the wake leg | `nvme_dev.rs:171`, `:233`, `:447`, `:745` | +15–20 % cold read | Acceptance bar already written: `fetch_dma − dev_service < 0.7 ms` |
| PERF-5 | Set io_uring setup flags on the FUSE queue rings — only `setup_cqsize` is set. The workers are already one-thread-per-ring so `SINGLE_ISSUER` is free, and `DEFER_TASKRUN` moves completion task-work onto the reaping thread's own `io_uring_enter`. The flags already exist in-tree | `fuse_over_uring.rs:1784`; cf. `zcrx_lane/uring_zcrx.rs:42` | 5–15 % syscall/wake economy | `read/write_transport_phase_ns` |
| PERF-6 | Set `FOPEN_KEEP_CACHE` — the kernel currently drops the page cache on every open despite `FUSE_WRITEBACK_CACHE` being negotiated. `AUTO_INVAL_DATA` is already on, which makes it safe (see **FUSE-4a**) | `fuse_client.rs:4039` | Potentially large on re-read workloads | `fuse_ops` READ count on a warm double-read |
| PERF-7 | Two `lseek64` syscalls per offsetful shim op — every `read`/`write` (the default for `cp`, `dd`, `tar`) pays them, against a design advertising zero syscalls per op. Interpose `lseek` and mirror the offset in the binding cell | `interpose.rs:531`, `:565`, `:609` | +30–60 % on `read()`-based drivers | `strace -c -f dd` (lseek → 0); elbencho pread-vs-dd pair |
| PERF-8 | Stop doing O(file-size) work on every layout save — the map is cloned, bincode-serialized whole just to compute `needs_indirect`, and `Arc::make_mut`-cloned again: three full passes per publish batch | `routing.rs:3595`, `:3605`, `:6988` | ~200 KB alloc+hash traffic per batch removed | `publish_phase_ns.{save_encode,apply}` |
| PERF-9 | Shard the indirect block map — currently the whole blob is re-serialized and a full block re-DMA'd per publish for any file above the inline cap. Removes the **RES**-adjacent size ceiling as a side effect and composes with **DUR-6** | `routing.rs:3625-3653` | 10–100× indirect-file publish bytes | `publish_indirect_blob_bytes ÷ user bytes`; `rewrite_amp` |
| PERF-10 | Eliminate the third copy on transformed blocks — `written_len` is essentially never 4 KiB-aligned, so every compressed or encrypted block takes the unaligned bounce path. Round up with an explicit re-check against `chunk_size` | `crypto_compress.rs:675`; `nvme_dev.rs:777` | 3 copies → 2 | `nvme_unaligned_write_fallbacks → ~0` |
| PERF-11 | Detach the tier publish from the read serve critical path — both admission arms await a multi-MiB mmap write before returning the caller's bytes, though the anti-churn rationale only requires it to precede the *guard drop* | `routing.rs:4502`, `:5289` | Removes a blocking-pool hop from every cold read | `read_fill_phase_ns[admission] → ≈0`; `read_tier_admissions` unchanged |
| PERF-12 | Remove per-op allocations from the kernel read and write handlers — the IPC lane was made allocation-free (worth 22–29 % IOPS); the kernel lane still pays 4–8 allocs/op, including `load_striped_block_keys`' Vec+clone+sort done twice (serve, then binding recheck) | `routing.rs:9244`, `:7747`, `:5634`; `fuse_client.rs:8021`, `:8012` | Comparable to the IPC campaign | `SQZ_ALLOC_TRACE=1` extended to the kernel path |
| PERF-13 | Remove the 5 ms admission-park tail — `Notify::notified()` with a 5 ms sleep backstop because `notify_waiters` stores no permit. Register the `Notified` before the re-check | `write_pipeline.rs:452`, `:526` | p99 `admit_wait` from ~5 ms to completion latency | `write_pipeline_phase_ns.admit_wait`; fio clat p99 |
| PERF-14 | Memoize the two hot-path `env::var` reads — an allocation plus the process-global environ lock, at 328 k publishes per row. The correct pattern is three lines away | `block_allocator.rs:886`; `routing.rs:6974` | Removes a serialization point | `perf` symbol time in `getenv` |
| PERF-15 | Sub-block framing for transformed volumes so `ranged_eligible` can drop the passthrough gate. Today a 4 KiB random read on a compressed volume fetches and decodes a whole block (≥1,024× amplification at 4 MiB), and the only signal is `ranged_reads == 0` | `routing.rs:5890`, `:9877` | Order of magnitude on compressed random reads | `ranged_reads > 0`; device bytes ÷ user bytes |
| PERF-16 | Delete the transport `pending` map — three sharded-mutex acquisitions per request. Carrying `(qid, ent_idx, commit_id)` in the request also structurally closes two **FUSE-2** rows | `fuse_over_uring.rs:164-219` | 3 mutex ops + 3 hashes per op | `read_transport_phase_ns.reply_commit` |
| PERF-17 | Raise the staged sub-image rider bound, or add an in-place ring patch for non-extending sub-image writes — currently a 1 MiB write into a 3 MiB staged file moves ~7 MiB | `routing.rs:8009`, `:8088`, `:7886` | ~3× fewer bytes on mid-size staged overwrites | `staged_rider_extent_writes` vs `staged_rmw_pooled_seeds` |
| PERF-18 | Header cache-line layout — `doorbell` (client RMW) shares a line with `daemon_parked` (daemon store); `CqeDoorbell.seq`/`parked` share 8 bytes. Both are opposite-side producer/consumer pairs written on every submit and every completion | `layout.rs:268-288` | qd1 clat; reaper rows | `perf c2c` on the header page |
| PERF-19 | Skip `sched_getcpu()` per op when NUMA is structurally a no-op (single-node boxes) | `ipc_host.rs:455`, `:478`; `numa_core.rs:419` | 1–3 % of a ~1 µs warm op | Warm il A/B on a single-node box; `numa_nodes` proves no instrument loss |
| PERF-20 | Per-service-thread severed pools — one global `ArrayQueue` head/tail for all threads is 2 contended CAS per write op at 12+ GB/s | `ipc_host.rs:530-533` | Ingest GB/s on the tcp substrate | `ipc_severed_pool_{hits,misses}` unchanged; `perf c2c` |
| PERF-21 | `SeqCst` → `Release`/`Acquire`-on-zero on the placed-assembly refcount — the only `SeqCst` in `src/` that is neither a shutdown flag nor a documented Dekker fence | `placed_sever.rs:150`, `:209`, `:241` | Small, free | — |
| PERF-22 | Precompute `min_distance[node]` in `numa_core` instead of an O(nodes) scan per classified copy pass | `numa_core.rs:226-232` | Small, free | — |
| PERF-23 | Trim clock reads — ~6 `Instant::now()` per transport op and ~10 per read serve. Consider 1-in-N sampling of sub-phases while keeping `total` unconditional, and re-measure the "invisible at any credible op rate" claim, which is currently asserted without a bracket | `fuse_over_uring.rs:2568`, `:2637`; `read_phase.rs:24-28` | ~100 ns/op | `write_transport_phase_ns.transport_total` |
| PERF-24 | Measure the untested `Q_DEPTH_DESIRED = 32` and `MAX_BACKGROUND_CEILING = 256` headroom — both documented as "beyond is unmeasured" while the L1 evidence shows the row scales with both gates | `fuse_over_uring.rs:988`, `:1007` | Cheapest unexplored lever | Depth 32/64 × max_background 256/512, device-true, sustained ≥ 60 s |
| PERF-25 | Re-derive `REAP_EVENT_PARK_MAX` after PERF-18 and the doorbell changes — it is a constant fitted to one venue and the campaign note flags it as a re-measure candidate | `interpose.rs:1983` | — | qd sweep × `ipc_cqe_wake_{writes,elided}`; the qd1 RTT row must be preserved |

---

## 10. Engineering process and tooling

| ID | Item | Priority | Required behavior |
|---|---|---|---|
| ENG-1 | `#![allow(clippy::all)]` at `src/lib.rs:1` and `src/main.rs:1` overrides `-D warnings`, so the authoritative gate inspects no clippy lints across 142 k LOC. Confirmed: a full run emits 0 warnings. A `--force-warn` re-run surfaced **158**, of which ~14 are correctness/suspicious class — including `not_unsafe_ptr_arg_deref` ×2 (**MEM-4**), `uninit_vec` ×2 (**MEM-6**), `suspicious_open_options` ×2 (`uring_fs.rs:1279`, `:1289`), `never_loop` (`zcrx_lane/initiator.rs:171`), and `if_same_then_else` (`uring_fs.rs:1031`). The remaining ~144 are style and complexity. The suppressed groups include `correctness` and `await_holding_lock`, and `arithmetic_side_effects`/`cast_possible_truncation` would have flagged **VAL-1** directly | **P0** | Remove the attribute. If the debt is too large for the RC, narrow to `#![allow(clippy::style, clippy::complexity, clippy::pedantic)]` so `correctness`, `suspicious`, and `perf` re-engage today. **Do this first — it may surface more items** |
| ENG-2 | `cargo audit` is not installed and appears in neither the full gate nor `task check`, so the AGENTS.md Phase-5 claim "no known vulnerabilities" has never been verified. `rsa 0.9.10` carries RUSTSEC-2023-0071 with no fixed release in the 0.9 line, and it backs the key-wrap path that **VAL-3** shows is itself broken | **P0** | Install, run, add to the gate, and adjudicate `rsa` in writing before the tag |
| ENG-3 | `env_logger::Builder::from_default_env()` with no filter default means the stock mount (no `RUST_LOG`) logs at **Error** only. Silently invisible: reservation preemption, the job-wire security notice, shard lease expiry, the O_DIRECT→buffered degradation (**DUR-2**), the bitmap-write failure (**DUR-4**), meta-volume teardown failure. Compounding: a failed `--log-file` open skips `builder.target()` inside `if let Ok(file)` with no error, and stdio was already redirected to `/dev/null` at `:2589` — so the daemon runs with all logging discarded | **P0** | Default to `info`; fail the mount loudly if `--log-file` cannot be opened; re-triage guard and fabric warnings for level |
| ENG-4 | `stamp_staging_dir` calls `remove_dir_all` on an operator-supplied path as root with no denylist, no emptiness or marker precondition, no confirmation, and no dry-run. Reachable from `format --disk-cache-paths` and `config set-cache-paths`. The function's own doc comment concedes the wipe is not required for correctness | **P0** | Refuse non-empty directories carrying no staging marker; hard-refuse a curated system-path list; require `--yes`; print the deletion plan |
| ENG-5 | Four unused direct dependencies — `hyper 0.14`, `hyper-rustls 0.24`, `zerocopy`, `twox-hash 1.6`, all with 0 references. `hyper-rustls` is the sole reason the binary links a **second, end-of-life TLS stack** (`rustls 0.21.12` alongside 0.23.40, plus webpki 0.101, sct 0.7, tokio-rustls 0.24, and a full HTTP/2 implementation) | **P1** | Delete. Four lines removes ~15 transitive crates and collapses 5 of 8 duplicate-version pairs |
| ENG-6 | `.cargo/config.toml` sets `[build] rustflags = ["-C","target-cpu=native"]`, contrary to *Portable by default*, and the file is picked up inside the distro build containers. Currently inert only because cargo does not merge `[build]` with `[target.<triple>]` and the target block wins — so deleting the `--no-rosegment` block, renaming the triple, or adding `--target` silently activates it across every dist artifact | **P1** | Delete the `[build]` entry |
| ENG-7 | No CI configuration exists (`.github/`, `.gitlab-ci.yml` both absent); the entire gate regime is honor-system | **P1** | Add a workflow that runs `task check` |
| ENG-8 | The default-features build — the shipped configuration — is never linted or tested; every gate is `--all-features`. And `--all-features` enables `dhat-on`, which replaces jemalloc and drops the `dirty_decay_ms` tuning the R5 comment calls load-bearing, so anyone benchmarking that build measures a different allocator | **P1** | Add `cargo clippy --all-targets -- -D warnings` as a second gate line; exclude `dhat-on` from measurement builds and document the exclusivity |
| ENG-9 | Tracked files that are not source: `test_db.rs` (a Redis-era scratch program; `redis` is no longer a dependency), `github.jpeg` (646 KB, unreferenced), and 16 `local-verify*.state` fio artifacts | **P2** | Untrack; add `*-verify.state` to `.gitignore` |
| ENG-10 | Env-knob parsing uses three incompatible conventions — 56 silent-default sites, 5 that `panic!` and kill the mount on a typo, 3 that return a loud error, and 19 boolean flags where `SQUEEZEFS_FREE_FORENSICS=0` **enables** the feature. Plus a prefix collision: `SQUEEZEFS_RECLAIM_BATCH` (inode reclaim) vs `SQUEEZEFS_RECLAIM_BATCH_BLOCKS` (block reclaim). ~20 knobs are documented nowhere | **P2** | One convention (loud refusal on a bad value); rename one of the colliding prefixes; document the missing knobs |
| ENG-11 | `SQUEEZEFS_IPC_ALLOW_DEV=1` relaxes the build-commit skew gate silently on both sides. `SQUEEZEFS_FUSE_NO_KILLPRIV` is the good precedent — it logs loudly | **P2** | Log on engagement |
| ENG-12 | `src/recovery.rs` is a stub returning `Ok(0)` with a comment about an in-memory store, and has no callers. AGENTS.md lists it as live crash-recovery machinery with fence and layout checks. The real machinery is `NvmeStaging::new` | **P2** | Delete; correct the documentation reference |
| ENG-13 | The p2p/DHT subsystem has no callers — `P2pServer::run` is unreachable and `dht_node` is never set, so both peer paths are permanently inert. ~500 lines including an accept-everything certificate verifier that would admit arbitrary peer bytes into the local read cache if ever wired up | **P2** | Delete, or gate behind an explicit feature with mandatory CA-pinned TLS |
| ENG-14 | Other dead items: `src/tiering/dht.rs:424,426` (written at construction, never read), `crates/fuse3/src/raw/session.rs:800` (`dispatch()`, zero callers), and the `dlm.rs` mock family. The 17 `abi.rs` allows are legitimate protocol completeness but want one documented module-level allow | **P2** | Delete; consolidate |
| ENG-15 | AGENTS.md drift: the L4 service-thread default is stated as `clamp(cpus/4,2,8)` and contradicted 400 words later by the correct `…,2,16)` — the stale figure is the constant a perf campaign was run to delete; ~~"Two Criterion benches" when there are four~~ (**closed Rev 3** — the 2026-08-04 microbench merge rewrote the section); the `cargo audit` mandate with no enforcement point; and the missing lock-order level 3.5 (**RES-21**). Of 21 defaults spot-checked, 18 were correct — the problem is coverage, not accuracy | **P2** | Correct the remainder |
| ENG-16 | Add `publish = false` to the root package (the `[patch.crates-io] fuse3` arrangement makes `cargo publish` a live footgun); add a `rust-toolchain.toml`; add `--locked` to the host build path, which the container path already uses | **P3** | — |
| ENG-17 | Add a test leg running `cargo test --all-features` inside `crates/fuse3` — the fork is `workspace.exclude`d, so its own `file-lock` feature (21 cfg sites) is compiled by no gate | **P3** | — |

---

## 11. Test infrastructure

| ID | Item | Priority | Required behavior |
|---|---|---|---|
| TEST-1 | **The durability contract has never been tested.** `uring_fs::arm_power_cut`/`power_cut` (`uring_fs.rs:355-419`) is a correct volatile-cache-loss simulator — it reverts every write not covered by an `fdatasync` — and it is used in exactly two places, both inside one self-test of the shim. Every other crash test is process death over a surviving page cache; the suites say so in their own headers (`kv_smo_crash_completeness_tests.rs:18-21`). `NvmeBlockDev` runs its own io_uring worker that never passes through the fault shim, so no existing harness *can* reach the data device. Combined with a test and benchmark fleet on zram, null_blk, and tempfiles — none with a volatile cache — a green gate carries no durability information | **P0** | Extend the fault shim to the `NvmeBlockDev` worker, tracking writes since the last data-device barrier. This is the prerequisite for **DUR-1**, **DUR-2**, **DUR-3**, **DUR-4**, **DUR-6**. Add at least one release-gate run on a substrate with a real volatile write cache |
| TEST-2 | 14 test files self-skip and report PASS when `/dev/fuse`, `fuse.enable_uring`, or `fusermount3` are absent — including the entire transport surface. `cargo test --all-features` can be all-green with the product's core mechanism unexecuted, and `eprintln!` skip notices are swallowed without `--nocapture` | **P1** | A `SQUEEZEFS_TEST_REQUIRE_MOUNT=1` mode turning each skip into a failure, wired into the release gate; a machine-readable skip ledger |
| TEST-3 | 77 bare fixed-duration sleeps in `tests/` (worst 8 s and 4 s), against the house rule forbidding sleep-as-synchronization. Under `--test-threads=1` these are both wall-clock and the classic source of load-dependent flakes | **P1** | Convert to the existing `poll_until`/`eventually` helpers |
| TEST-4 | No fuzz targets exist and there are 7 proptest blocks tree-wide, against three untrusted-input surfaces: the IPC shared memfd, remote job-wire frames, and on-disk metadata. `tests/ipc_host_tests.rs` covers every *named* refusal class well; what is missing is unstructured input | **P1** | `cargo-fuzz` targets for the bootstrap blob, ring headers, the job-wire frame decoder, and the five on-disk decoders; proptest round-trip and never-panic-on-arbitrary-bytes for `kv/{record,node,bset,journal}.rs` and `layout_wire.rs` |
| TEST-5 | `crates/squeezefs-preload/src/fd_table.rs` has no loom model — a 9-CAS lock-free table running inside arbitrary host applications with fork semantics, 329 LOC, zero in-module tests. Also unmodeled: `sync_coalescer.rs` (11 atomics, 1 referencing test file), `node_cache.rs` (19 atomics) | **P1** | Extract `fd_table_core.rs` per the existing 19-core convention and model it |
| TEST-6 | ~~`src/zcrx_lane/uring_zcrx.rs`: 869 LOC, 18 `unsafe` sites, **zero test references anywhere**~~ (**closed** — the in-module contract suite `src/zcrx_lane/uring_zcrx.rs::tests`, 19 tests: the two pure per-completion gates + a never-panic proptest over kernel-provided CQE words (`decoder_property_tests` law), raw-ring setup/CQE32 round-trip/index-wrap-across-capacity/SQ-full bound, refill-ring rqe encode + counter-wrap, the `REGISTER_ZCRX_IFQ` argument law (kernel-out params zeroed; region page-round), the `SlotBag`/`SendBufs`/adopt-custody exit-path laws, ring-fd-close-with-op-in-flight teardown, and the universally-reachable ifq-refusal + poison legs; every unsafe site is exercised or its SAFETY invariant is upheld structurally by one of these, and environment skips route through the testkit ledger. The armed serve loop stays field-owed — the D5 chain's remaining link is the Z3 field rows) | **P1** | Cover before the lane ships default-on |
| TEST-7 | Two open, reproducible, unowned dev-tip flakes: `multi_queue_tests::storm::…_no_starvation` (2/5 on untouched tip, 5/6 under load) and `volume_drain_tests::test_offline_remove_data_drains_to_retired` (2/4). Both are load-selected schedules in transport and drain — the class the project classifies as first-class product bugs. Three campaigns have filed them; none owns them | **P1** | Assign and fix |
| TEST-8 | The three-suite release gate is green on binary `f5468ed` — roughly five weeks and one on-disk format change stale. Dev has since taken rewrite P0, publish commit aggregation, dynamic meta routing (incompat bit 6), the transport geometry rewrite, NUMA affinity, the read lane and hold probe, NT read serve, and zcrx Z1/Z2 — and, Rev 3, the five 2026-08-04 campaigns (`68e8474..391dec2`: SDK Tier-1 direct-link, reset-v5 prep incl. the bit-62 daemon arm, the derivation sweep's 11 knob conversions, the microbench program, the volume-drain claim-release fix) | **P0** | Re-run from zero on the RC binary, per the project's own rule |
| TEST-9 | Coverage gaps by module: `src/tiering/dht.rs` (923 LOC, 2 test references), `src/meta_backend/sync_coalescer.rs` (377 LOC, 1), `src/storage.rs` (832 LOC, 1), `src/job_wire.rs` (2,028 LOC, no in-module tests), `src/config_ops.rs` (1,993 LOC, no in-module tests), `src/supervisor.rs` (1) | **P2** | Prioritize `job_wire` and `sync_coalescer` |
| TEST-10 | Crash/kill coverage is otherwise **strong** — 7 dedicated files plus SIGKILL usage in 12 more, with torn-write handling covered. Only 4 tests are `#[ignore]`d, all with explicit reasons. No action needed beyond **TEST-1** | — | Preserve |

**Evidence-practice items.** There is no current metadata number (the headline storm figures are from 2026-07-15 on a box explicitly labeled DIRTY with a co-tenant at loadavg 19–25, predating every meta-plane change since). The competitive scoreboard predates the reset-v3→v4 epoch change and ~20 campaigns. Dynamic meta routing shipped an on-disk incompat bit on in-process, debug-build, file-backed evidence only, and is now the change blocking all field validation, so the last three campaigns have zero field evidence behind one reformat window. The sustained-state rule is not universally honored (the zcrx GO decision rests on 30-second rows). `write-wall` §6.6's structural verdicts are zram-target-specific while reset-v4 nullblk performs 2.6× better on the same shape. A retracted finding (the serve-decomposition "5.3 ms pre-handler prize") is still readable as current. And no RC manifest exists — the merged-SHA × field-validated state is not reconstructible from `.benchmarks/`.

---

## 12. Suggested sequencing

**First, because it changes what is known**

0. **ENG-1** — remove the clippy suppression. The forced run says the debt is 158 warnings, mostly cosmetic, and the group most likely to be broken (`await_holding_lock`) is already clean. The cost is far lower than it appears, and two of the correctness lints bear directly on **MEM-1**, **MEM-2**, and **VAL-1**.

**Then the P0 items, roughly by reachability**

1. **VAL-1** — bounds-check the ioctl arguments and cap the key loop.
2. **VAL-2** — the xattr allowlist, mirrored into the backend. One edit covers all four internal records.
3. **VAL-3** — get the key material off the volume and off argv; fix the path-vs-content contradiction.
4. **VAL-4** — shim-side peer and seal verification; harden the socket directory.
5. **MEM-1 + MEM-2** — ownership for the zero-copy read destination and the assembly tasks.
6. **TEST-1 → DUR-2 → DUR-1** — build the data-device power-loss harness first, then add the flush primitive, then fix the fsync path. In that order, because the first two are untestable without the harness.
7. **DUR-3 + DUR-4** — epoch-stamp `pending_reclaim`; restore the bitmap dirty set on failure.
8. **FUSE-1** — the minor-version bump and `FUSE_INIT_EXT` together, with a negotiation test asserting both.
9. **VAL-5 + VAL-6** — the IPC and job-wire bounds, and a decision on the job-wire posture.
10. **ENG-3 + ENG-4** — make the daemon audible; guard the cache-path wipe.
11. **FUSE-2** — the `fail_ent` helper, the ungated watchdog, and the must-stay-0 counter.
12. **DUR-5 + DUR-6 + DUR-7** — superblock redundancy, blob integrity, cross-volume atomicity.
13. **TEST-8** — re-run the release gate from zero on the RC binary; **TEST-7** — own the two flakes.

**Cheap, same pass**

**ENG-5**, **ENG-6**, **ENG-9**, **ENG-12**, **ENG-13**, **ENG-14** (deletions and dependency hygiene); **RES-4** (three lines); **RES-1**, **RES-2**, **RES-3**, **RES-7**; **PERF-14**; **POSIX-6** (before applications depend on the errno mapping); **ENG-2**; **TEST-2**.

**The DLM program**

Per §6.9: **S0–S3** are cleanup and hardening that deliver value independently — they close the unbounded-growth item, make fencing remount-monotone, and resolve **VAL-6**. **S4** is the gate: solo mode must be indistinguishable from today's performance with `dlm_rpcs == 0`. **S5** — one writer plus N coherent readers — is the increment to target for the release after this one, and notably requires no distributed lock manager, only a revalidation cadence and a freed-offset grace period. Full multi-writer (**S8–S11**) should be planned as its own program with §6.2's durable-format work sequenced first.

**Before any of it:** resolve **§6.12** — decide what the scale target means and correct the documentation to describe the shipped product.

---

## Appendix: method and coverage

Nineteen read-only passes across two rounds. Round one: unsafe/memory safety, the LD_PRELOAD shim, the IPC host and ring protocol, the fuse3 transport fork, metadata KV v3, the write path, the read path, operational robustness, POSIX semantics, and architecture/performance ceiling. Round two: a distributed-lock-manager forensic census, a comparative design study, scaling arithmetic, multi-client coherence, concurrency and resource lifecycle, durability-chain verification, metadata crash-window verification, a second security-validation pass, and a second transport/POSIX pass.

Verification performed directly by the reviewer for all **[V]** items, including: the clippy suppression and the `--force-warn` result; the FUSE minor version and `FUSE_INIT_EXT` absence; the absence of any `Fsync` op in `nvme_dev.rs`; the ioctl overflow arithmetic and the amplifier loop; the xattr screen's coverage versus the four internal record names; the encrypt-key path-versus-content contradiction; the unconditional `flock(LOCK_EX)` at mount; the non-durable block refcounts; the never-incremented lease counters; the `Bytes::from_static` ownership gap and its timeout; the `try_join_all` detachment semantics; the dependency and `.cargo/config.toml` state; and the tracked non-source files.

No files in the repository were modified during this review.
