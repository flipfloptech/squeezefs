# SqueezeFS WRITE-path byte-copy audit (large / striped path)

Read-only audit of `/home/justin/Source/squeezefs` @ 2026-07-07.
Scenario: large sequential write to a striped file. Defaults: FUSE `max_write` = 1 MiB
(`KERNEL_MAX_PAGES_LIMIT * 4096`, `third_party/fuse3/src/raw/connection/fuse_over_uring.rs:268-273`),
FS `block_size` = 4 MiB. Crypto/compression **passthrough** unless noted.

## Path actually taken by a large sequential write

Critical routing fact first: `src/fuse_client.rs:2847-2849` gates the "aligned direct striped"
path on `offset % block_size == 0 && data.len() % block_size == 0`. With 1 MiB FUSE writes and a
4 MiB block, `data.len() % block_size != 0` — **`is_aligned` is false for every request**, so the
entire sequential stream funnels through `write_file_staged` (RMW buffer → NVMe staging mmap →
background writeback → block backend). That is why staging copies dominate the profile even for
"perfectly sequential" workloads.

```
kernel page cache
  ─K→ ent.payload (registered uring payload buf)        [kernel copy, COMMIT_AND_FETCH]
  ─1→ Bytes (fresh heap Vec, per request)               fuse_over_uring.rs:896
  ─2→ session data_buffer (reused Vec)                  tokio.rs:473-477
  ──  fs.write(payload: Bytes)                          session.rs:2469-2489  (refcount, no copy)
  ─3→ active block buffer (ALIGNED_BUF_POOL, 4 MiB)     fuse_client.rs:1384-1390
  ─4→ staging mmap segment (+ msync → staging NVMe)     tiering/nvme.rs:439,452
  ─5→ Bytes (fresh 4 MiB heap Vec) in writeback         fuse_client.rs:5600
  ──  crypto passthrough (no copy)                      crypto_compress.rs:350-354
  ─DMA→ block device (io_uring, zero-copy submit)       nvme_dev.rs:573-592
```

Every payload byte is CPU-copied **5× in userspace** (plus 1 kernel copy, plus a full extra
*device write* to staging NVMe before the real one). 430 MiB/s × ~6 copies ≈ 2.5 GB/s of memcpy
traffic — consistent with 45-50% of cycles in glibc memcpy and a 2.1 GB/s substrate.

---

## (a) Copy inventory — numbered

Sizes are per unit that flows through that point (R = one FUSE request ≤1 MiB; B = one 4 MiB block).

| # | Location | From → To | Size | Avoidable? How |
|---|----------|-----------|------|----------------|
| K | kernel `fuse_uring_commit_fetch` (not in repo) | page cache → registered `ent.payload` (`fuse_over_uring.rs:680,704-707`) | 1 MiB /R | **No** (required kernel→user copy; the one physically necessary CPU copy) |
| 1 | `third_party/fuse3/src/raw/connection/fuse_over_uring.rs:896-898` | `ent.payload: Vec<u8>` → `Bytes::copy_from_slice` → fresh heap `Bytes` | 1 MiB /R **+ 1 MiB alloc per request** | **Yes.** `Bytes::from_owner` over a leased ent-payload buffer; double-buffer per ent or defer COMMIT re-arm until the lease drops |
| 2 | `third_party/fuse3/src/raw/connection/tokio.rs:470,475` (also 483,494 fallback) | `InboundUringReq.payload: Bytes` → session `data_buf: Vec<u8>` (reused) | 1 MiB /R | **Yes.** Only `op_in` (= `fuse_write_in`, 40 B) is needed by `handle_write`; skip body copy when opcode == FUSE_WRITE (opcode is at `header_and_op[4..8]`) |
| 3 | `third_party/fuse3/src/raw/session.rs:2471` | `Bytes::copy_from_slice(data)` — **classical path only**; uring path (`:2469-2470`) reuses `payload` refcounted | 1 MiB /R (classical only) | Already zero-copy over-uring; classical is INIT-only, ignore |
| 4 | `src/fuse_client.rs:1262-1270` | first touch of a block: `ALIGNED_BUF_POOL.alloc_raw()` + `write_bytes(ptr,0,4MiB)` zero-fill | 4 MiB /B (memset) | **Yes** for full-coverage blocks: no buffer needed at all (see refactor 1) |
| 5 | `src/fuse_client.rs:1358-1371` | RMW seed: existing block `Bytes` → `copy_nonoverlapping` into `AlignedBufOwner` | ≤4 MiB /B | Only on partial overwrite of existing data; unavoidable for true RMW, avoidable when coverage is complete |
| 6 | `src/fuse_client.rs:1384-1390` | request payload slice → `copy_nonoverlapping` into block buffer (`block_data.as_ptr() as *mut u8` — **mutates a shared `Bytes` via raw ptr; latent UB**) | 1 MiB /R (4 MiB cumulative /B) | **Yes** for full-coverage blocks (refactor 1). For genuine partial-block RMW this is the legitimate merge copy |
| 7 | `src/cache/nvme.rs:627-651` → `src/tiering/nvme.rs:439` (`reserve_and_write`) | block buffer `&[u8]` → `copy_nonoverlapping` into staging **mmap** segment + `msync(MS_ASYNC)` (`tiering/nvme.rs:452-456`) | 4 MiB /B **+ 4 MiB staging device write** | **Yes** for sequential complete blocks: write through to the block backend instead of staging (refactor 1). Staging is only needed for partial blocks / backpressure |
| 8 | `src/fuse_client.rs:5595-5601` (`flush_single_active_block`) | staging mmap (`read_staged_zero_copy` guard) → `Bytes::copy_from_slice` → fresh 4 MiB heap `Bytes` | 4 MiB /B + 4 MiB alloc | **Yes.** `Bytes::from_owner(guard)` — mmap is stable & 4 KiB-aligned, feeds `write_block`'s aligned zero-copy branch directly (refactor 3) |
| 9 | `src/crypto_compress.rs:350-354` (`process_write`) | passthrough: returns same `Bytes` — **zero copy** ✅ | 0 | n/a. Non-passthrough (`:355-361`): lz4/zstd → new `Cow`/`Vec`, then AES → second `Vec` (1-2 unavoidable transform buffers) |
| 10 | `src/nvme_dev.rs:573-592` (`write_block`) | 4 KiB-aligned `Bytes` → io_uring submit with `_keep_alive` — **zero copy** ✅ (jemalloc 4 MiB allocs are page-aligned, so #8's Bytes and `BUFFER_POOL` Vecs hit this branch) | 0 | n/a |
| 11 | `src/nvme_dev.rs:597-616` / `:625-641` | unaligned fallback: `memcpy` into `ALIGNED_BUF_POOL` buf or `posix_memalign` buf | ≤4 MiB /B | Alignment is not *guaranteed* by contract, only by jemalloc behavior — make it contractual (aligned pools) so this branch is provably cold |
| 12 | `src/routing.rs:2054-2058` (direct striped path, when it *is* taken) | `data_slice` (`slice_ref`, zero-copy at `:1992-1997`) → `copy_from_slice` into `BUFFER_POOL` 4 MiB `PooledBuf` | 4 MiB /B | **Yes** when the overlap covers the whole block: use `data_slice` directly as `block_bytes` (`into_bytes` at `:2065` is already zero-copy `from_owner`) |
| 13 | `src/routing.rs:2022-2027,2032-2037` | `ReadBlockValue::Bytes` → `BUFFER_POOL` pooled copy (RMW seed on direct path) | ≤4 MiB /B | Partial-RMW only; same status as #5 |

Allocator-pressure notes (jemalloc in profile):
- **1 MiB heap alloc per FUSE WRITE request** from #1 (`Bytes::copy_from_slice`) — the single biggest per-request allocation.
- **4 MiB heap alloc per block** from #8.
- Per-request small allocs: `header_and_op` Vec (`fuse_over_uring.rs:892-895`, ~56 B), reply Vec (`session.rs:2512`), `data.deref().to_vec()` in the uring reply path (`tokio.rs:590`, 24 B), key `Bytes::copy_from_slice(key.as_bytes())` throughout `src/cache/nvme.rs` (636, 681, 705, 724…).
- Pools that already exist and should be leaned on: `BUFFER_POOL` (4 MiB Vecs, `src/cache/pool.rs:83-89`), `ALIGNED_BUF_POOL` (4 KiB-aligned, `pool.rs:158-190`).

## (b) Minimal-copy ideal

Physically required for a FUSE write to an NVMe block backend:

1. **1 CPU copy (kernel):** page cache → userspace payload buffer during `COMMIT_AND_FETCH` (copy K). FUSE-over-io_uring offers no splice; this is irreducible today.
2. **0 CPU copies (device):** io_uring DMA's straight from that user buffer (nvme_dev already supports this via `WriteData::Aligned` + `_keep_alive: Bytes`).

So the ideal hot path is **1 copy total**: kernel → ent.payload → (crypto transform if configured) → DMA. For genuine partial-block RMW, +1 merge copy is legitimate. Current state: **5-6 copies + a full extra staging device write per block.** Everything between copy K and the DMA is protocol-internal shuffling.

## (c) Top 3 refactor candidates (ranked by expected MiB/s recovered)

### 1. Complete-block write-through: skip staging for fully-covered blocks (est. +400-700 MiB/s)
Eliminates copies **#4, #6, #7, #8** *and* the redundant 4 MiB staging device write + `msync` per block — for sequential streams every block is fully covered, so this removes ~16 MiB of memcpy/memset + 4 MiB of extra NVMe traffic per 4 MiB written.
Changes:
- `SqueezefsFilesystem::write_file_staged` (`src/fuse_client.rs:1200`): inside the per-block future, when `write_start == b_start_offset && write_end == b_end_offset` (full coverage — not just full request alignment), bypass buffer/staging entirely: take `data.slice(data_cursor..data_cursor+slice_len)` → `crypto.process_write_async` → `nvme_writer.write_block` → merge block_map (i.e. inline what `flush_single_active_block` does, minus staging), invalidating `active_block_buffers` / staging for that key.
- Alternatively/additionally, fix the gate at `src/fuse_client.rs:2847-2849`: replace request-granularity `is_aligned` with per-block coverage so covered blocks route through the existing direct striped path in `DataRouter::write_file` (`src/routing.rs:1462`, striped section `:1946-2087`).
- Keep staging strictly for partial/tail blocks and backpressure spill (its actual design role).
Signature impact: none externally; new private helper e.g. `async fn upload_full_block(&self, ino: u64, b: u32, data: bytes::Bytes, fencing_token: u64) -> Result<(), SqueezefsError>`.

### 2. Transport zero-copy: kill copies #1 and #2 (est. +150-300 MiB/s)
2 MiB of memcpy + a 1 MiB heap alloc per 1 MiB request, on the fuse-over-uring thread — this is the transport half of the glibc-memcpy profile.
Changes:
- `third_party/fuse3/src/raw/connection/fuse_over_uring.rs`: replace `Bytes::copy_from_slice(&ents[ent_idx].payload[..])` (`:896-898`) with `Bytes::from_owner(EntPayloadLease { pool, qid, ent_idx })`. Requires the ent's payload buffer not to be re-armed while the lease lives: either (a) per-ent double buffering (swap a spare `Vec<u8>` in before COMMIT_AND_FETCH — moves the copy to only-when-contended), or (b) defer the ent's re-arm until lease drop (depth=4 gives slack; add a "parked ent" state). Note `UringBufOwner` (`tokio.rs:576-579`) already proves the from_owner pattern on the reply side.
- `third_party/fuse3/src/raw/connection/tokio.rs:462-499`: after `header_buf` is filled, read opcode from `inbound.header_and_op[4..8]`; for `FUSE_WRITE`, copy only `op_in` (40 B of `fuse_write_in`) into `data_buf` and skip the payload body copy — `handle_write` (`session.rs:2436-2472`) only parses `fuse_write_in` from `data_ref` and then uses `uring_payload` anyway. (Requires relaxing the `write_in.size == data.len()` check at `session.rs:2461` to validate against `payload.len()` on the uring path.)

### 3. Writeback flush zero-copy: kill copy #8 (est. +100-200 MiB/s, and the fsync path)
Partially subsumed by refactor 1 for sequential streams, but still hit by partial blocks, backpressure spills, and every fsync-driven flush; also removes a 4 MiB alloc per block.
Changes:
- `flush_single_active_block` (`src/fuse_client.rs:5565`): replace `:5595-5601` (`read_staged_zero_copy` → `Bytes::copy_from_slice(&guard)`) with `Bytes::from_owner(guard)`. `NvmeCacheReadGuard` (`src/tiering/nvme.rs:94-106`) derefs to the mmap slice, which is stable and 4 KiB-aligned → `write_block` (`src/nvme_dev.rs:573`) takes the `WriteData::Aligned` zero-copy DMA branch with the guard as `_keep_alive`.
- Requires `NvmeCacheReadGuard: Send + Sync` (it holds a `RwLockReadGuard<'static, NvmeShardInner>`; audit lock-hold-across-await against the staging eviction path — an `Arc`-pinned segment refcount is the fallback design).
- Same trick applies to `promote`/upload paths in `src/routing.rs` that currently use `read_staged` (`src/cache/nvme.rs:680-701`, which is a `to_vec()` full copy: `:695`).

### Honorable mentions
- Fix the shared-`Bytes` raw-pointer mutation at `src/fuse_client.rs:1383-1390` while touching refactor 1 (correctness, not just perf).
- `src/routing.rs:2054-2058` (copy #12): use `data_slice` directly when overlap == whole block.
- Make 4 KiB alignment contractual for `BUFFER_POOL` (`src/cache/pool.rs:15,21` uses plain `vec![0u8; 4MiB]`) so `nvme_dev.rs:573`'s aligned branch is guaranteed, not jemalloc-luck.
