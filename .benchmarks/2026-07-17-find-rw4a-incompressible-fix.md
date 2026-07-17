# FIND-RW4-A fix — incompressible blocks round-trip on compressed volumes (2026-07-17)

**Charter**: close FIND-RW4-A (`.benchmarks/2026-07-17-rw4-extent-overlay.md`
§6, A/B-proven pre-existing on base `a070836`): lz4-compressed volumes could
not hold incompressible full-size blocks — a beta-blocker-class
data-availability bug on a shipped feature (`--compression lz4|zstd`,
`--encrypt-algo`). RW4's G-RW6 acceptance had to dodge it with fio
`--buffer_compress_percentage=75`; this note re-runs that row on
fully-random data with zero crutches.

## Provenance

| | |
|---|---|
| Tree | `fix/compress-incompressible-blocks` off dev `22d652a` (RED `25918cb` → impl `55487bb` → this note); release md5 `e53739b6cecd9170fada585be87492fb`, built `taskset -c 0-15 CARGO_BUILD_JOBS=12` |
| Box | the RW4 box (25 CPUs, nvme0n1, kernel 7.1.3-2-cachyos) |
| Rails | fresh sandbox `/var/tmp/squeezefs_rw4a_fix` (never `/mnt/squeezefs`, `/mnt/juicefs`, `~/tmp/nvme`); daemon in a `systemd-run --user --scope` **8 GiB** memcg cage, `taskset -c 0-15`; 3-poll quiet gate before every timed row; kills by PID; artifacts preserved (`…/artifacts/20260717T070419Z`, plus `smoke/`) |
| Instrument | fio 3.42, **`--refill_buffers`, NO `--buffer_compress_percentage`** — fully-random (incompressible) payloads, the elbencho payload class that surfaced the finding |

## 1. Exact defect mechanism (stated per the charter)

Three cooperating pieces, all pre-existing on `a070836..22d652a`:

1. **No expansion escape in the transform.** `process_write` framed every
   non-passthrough image `[u32 LE image_len][image]` with no fallback:
   incompressible payloads EXPAND under lz4/zstd (lz4 worst case for a
   4 MiB block ≈ 4,210,789 B including the frame; the A/B measured a
   4,210,758 B claim), and the AEAD envelope adds 291–547 B
   unconditionally — so a full `block_size` payload's stored image
   exceeded `block_size` on **every** compressed and/or encrypted volume.
2. **The write landed anyway — by OVERFLOWING the allocator chunk, never
   by truncation.** Every durable-upload site (`upload_full_block`,
   `fold_upload_block`, `upload_active_block_bytes`,
   `write_block_from_staging`, `durable_write_sparse_blocks`, the striped
   RMW leg, spill, truncate clip) DMA'd the transformed image at
   `allocate_block()`'s offset with **no size check**, and
   `NvmeBlockDev::write_block` writes whatever it is handed. Offsets are
   minted as `block_idx × chunk_size` (chunk = 4 MiB), so on the default
   geometry (`block_size == chunk == 4 MiB`) the image's tail landed in
   the **next chunk's territory** — silent cross-tenant corruption, last
   writer wins. (`promote_staged_file` was the one site with a `> chunk`
   check; it silently stayed ring-resident.)
3. **The read window was too small to ever read the image back.**
   Undecorated block reads fetch exactly `block_size` bytes; the frame
   parse then claims more than the window holds and every read fails loud:
   `malformed transform frame: claims 4210754 image bytes, 4194300
   available` (the A/B signature — 24/24 files on a 1 GiB elbencho
   create, zero kills involved). Size-decorated `bk:0:len` mappings
   (spill/promote/clip) read the exact image and could decode — until a
   neighbor-chunk write from (2) trampled the overflow tail.

Inline and staging-ring audit (charter item): inline payloads live
untransformed in the metadata layout value and the staging ring stores
plaintext mmap segments — no expansion hazard at either store site. The
transformed staged sites (promote/spill/clip) are covered by (2)/(3).

## 2. TDD evidence

- **RED** (`25918cb`, M1 convention — assertions, never compilation):
  `tests/crypto_incompressible_tests.rs`, **11/13 red on dev** with the
  exact defect signatures: lz4 / zstd / lz4+aes / zstd+chacha / aes-only
  striped round-trips all failed their first post-fsync read
  (`Errno(22)` ← the frame error; write-through means even "warm" reads
  of incompressible blocks were device-backed), the RW4 fold path failed
  its fold-seed read at fsync, the raw-marker decode + counter + mount
  geometry-gate + chunk-overflow contracts failed their refusal
  assertions. The 2 non-red are the compressible/passthrough controls.
- **GREEN** (`55487bb`): 13/13, plus the full serial gate — clippy
  `-D warnings` clean, fmt clean, `cargo test --all-features --
  --test-threads=1` **929 passed / 0 failed across 97 targets**, doc 0
  warnings, bench smoke ok (includes the `CryptoCompressState`
  micro-benches). No lock-free core touched ⇒ no loom delta.

## 3. What landed (fix design — frame marker + the layer it sits at)

- **Store-raw escape** (`src/crypto_compress.rs`): bit 31 of the frame
  word (`FRAME_RAW_FLAG`) = "payload stored RAW". Both writer legs
  (pooled scratch + heap) compress first and keep the compressed image
  only when it SHRINKS (`compressed < raw`); otherwise the raw payload is
  stored behind the marker. Compression is best-effort per block — the
  btrfs/zfs posture. The escape sits **below the AEAD layer**: encrypted
  volumes store `encrypt(raw)` with the marker, and `process_read`
  dispatches on the marker after decrypt — never guesses.
- **Normative bound**: `max_stored_image_len(payload) = frame(4) +
  conservative AEAD envelope (3 + 512 + 12 + 16, RSA-4096 fallback) +
  payload`, debug-asserted at both writer legs.
- **Geometry** (`format` + both mount gates): transformed volumes reserve
  `TRANSFORM_BLOCK_HEADROOM` (4096 B, one LBA; const-asserted ≥ the
  worst-case non-payload bytes, 547 B) inside each chunk — `format`
  clamps `block_size` to `CHUNK_SIZE − 4096 = 4,190,208` loudly and
  refuses `block_size > chunk` outright; the mount bootstrap AND FUSE
  init REFUSE transformed volumes whose geometry cannot hold
  `max_stored_image_len(block_size)` (== every pre-fix
  compressed/encrypted format). Passthrough volumes are untouched
  (byte-identity, R3 ranged reads, zero-copy raw-DMA leg all unchanged).
- **Widened read window** (`DataRouter::device_block_window`):
  non-passthrough undecorated block reads fetch
  `round4k(max_stored_image_len(block_size))` clamped to the chunk
  (4 MiB on the clamped default geometry — one extra LBA per block read).
- **Defense in depth** (`block_allocator::ensure_stored_block_image_fits`):
  every transformed upload site refuses an image `> chunk` with a loud
  EIO — **never silent truncation, never an overflow write**. Deliberately
  an error, not an assert: a mis-geometried volume must degrade loud, not
  abort the daemon. `promote_staged_file`'s stay-resident defer (the
  never-wrong choice for that opportunistic path) now logs `error!`.
- **Observability**: `compress_stored_raw` (stats inode) — raw-escape
  stores; ≈ 0 on compressible workloads, ≈ upload count on random ones.

## 4. Compat posture shipped (both halves, per the charter)

- **Decoder = strict superset of the pre-fix encoding.** Stored images
  are MiB-class, so bit 31 was always 0 in every pre-fix frame; unflagged
  frames decode byte-identically through the new path (pinned by
  `frame_marker_superset_of_pre_fix_encoding`, which constructs the old
  form). Nothing that ever decoded stops decoding.
- **Pre-fix transformed GEOMETRY refuses loud (forward-only).** Every
  pre-fix compressed/encrypted volume was formatted with `block_size ==
  chunk`, which cannot hold the worst-case stored image — mounts now
  refuse with a reformat message naming FIND-RW4-A. This is the honest
  posture: such volumes' incompressible blocks were never readable and
  their overflow writes may already have corrupted neighbors. Pre-fix
  transformed volumes with SMALL block sizes (≤ chunk − headroom), had
  any existed, pass the gate and strictly improve: their compressible
  blocks decode unchanged and their previously-unreadable expanded
  blocks become readable through the widened window.
- v3 metadata format untouched (block-payload framing + a config-value
  clamp only).

## 5. G-RW6-shaped row WITHOUT the crutch (the RW4 acceptance re-run)

lz4 volume (4×12 GiB data slices, block size clamped 4,190,208 B),
16 GiB dataset over 16 files, fio randwrite 4 KiB t16 iodepth16, 30 s
rows, n=3, **fully-random payloads** (`--refill_buffers`, no
`--buffer_compress_percentage`) — the exact shape that was IMPOSSIBLE
before this fix (the A/B's 24/24 frame errors):

| row | IOPS | user MiB | dev R+W amp (row window) | +sync settle |
|---|---:|---:|---:|---:|
| r1 | **42,184** | 4,971 | 6.57× + 13.52× ≈ **20.1×** | 20.2× |
| r2 | **29,111** | 3,425 | 6.02× + 20.07× ≈ **26.1×** | 26.3× |
| r3 | **35,354** | 4,156 | 6.45× + 18.97× ≈ **25.4×** | 25.5× |

- **Zero `malformed transform frame` errors** across format → prep
  (16 GiB incompressible seq) → 3 storm rows → live full read-back →
  kill-9-recovery remount → **cold 16 GiB read-back (0 read errors,
  0 frame errors)**. The remount was a genuine crash-recovery pass (the
  first daemon was SIGKILLed mid-drain by the rig's impatient timeout —
  recovery swept, custody laws applied, everything readable).
- **Amplification 20–26× combined** — ~6× inside RW4's ≤150× G-RW6 gate
  and consistent with the design's §4 no-compression arithmetic
  (amp ≈ 2048/k + spill legs ≈ 28× predicted at k ≈ 80; measured mean
  fold fill **98.7**). RW4's 15–17× row got its extra ~4:1 from lz4 on
  75 %-compressible payloads; nothing compresses here **by design of the
  test** — that is the honesty line, not a regression signal.
- **IOPS honesty**: 29.1–42.2 k vs RW4's 17.7–18.3 k is NOT
  apples-to-apples (different payload generator, and raw-escape uploads
  skip the lz4 compression of incompressible data — the slowest lz4 case
  — replacing it with a memcpy). The row's gates are the amplification
  bound and the zero-frame-error mandate; both hold with margin.
- **Ledger truth** (per row): `extent_parks` 618 k–1.03 M ≈ ops,
  `fold_seed_reads == fold_passes` exactly (5,133–6,031), mean fill 98.7,
  `patch_writes = 0`, `extent_implicit_escalations = 0` — the RW4
  machinery shape, now on incompressible data. `compress_stored_raw`
  **19,021** over the session ≈ fold uploads + spills (every stored
  image of random data took the escape); the compressible smoke's
  counter stayed 0 while its stored image shrank (both directions
  pinned in cargo too).

## 6. fstests

`FSTESTS_QUICK=1 sudo tests/run_fstests.sh` on the branch tip: **exact
match with the expected table** — failures {generic/003, generic/213},
not-run {generic/009, generic/316}, all 15 others pass.

## 7. Residuals / notes

- `jobs.rs::BlockMove` copies raw stored images at a caller-specified
  `len`; its only producers today are tests (defrag is a stub). When a
  live producer appears it must size moves by the stored image
  (`bk:0:len` / chunk), not `block_size` — noted here so the owner
  inherits the constraint.
- The format summary prints the clamped size as "4.00 MiB"
  (`format_size_human` rounding of 4,190,208); the clamp itself prints
  the exact byte value on the line above. Cosmetic.
- `squeezefs clone`'s CLI reconstructs allocators outside the mount
  gates; it shares refcounts rather than rewriting block payloads, and
  every write path it can reach carries the chunk guard — pre-fix
  transformed volumes still refuse at mount, which is where clone
  sources/destinations are used.

## 8. Gate adjudication

| Gate | Verdict | Evidence |
|---|---|---|
| Incompressible round-trip (lz4/zstd ± encryption; whole-block, fold, warm+cold, crash pin) | **CLOSED** — 13/13 cargo contracts green (11 red on dev); 16 GiB fully-random volume round-trips through kill-9 recovery with 0 frame errors | §2, §5 |
| G-RW6-shaped row without the compress crutch | **PASSED** — 29.1–42.2 k IOPS, 20–26× combined amp (≤150× gate), zero frame errors | §5 |
| Compressible control (escape must not disable compression) | **PASSED** — `compress_stored_raw` 0 + stored image < raw (device-truth frame parse) | §2, §5 |
| Cargo gate | clippy/fmt/doc/bench-smoke clean; 929/0 across 97 targets | §2 |
| fstests QUICK | expected table exactly ({003,213} fail, {009,316} notrun) | §6 |
