# sqzhash128 hash and its adoption in squeezefs-zig
The owner's keyed AES-based 128-bit hash (API, properties, limits), how squeezefs-zig uses it since format v5 (`src/core/sqzhash.zig`, the `checksum.zig` facade, FORMAT_KEY, the bounded streaming Hasher), and the measured numbers.

## Reference implementation
- Lives at `~/Source/zig_hash_bench/src/sqzhash.zig` (~793 lines), exposed as Zig module `sqzhash` in that repo's `build.zig` plus a C-ABI static lib; C ABI in `src/sqzhash_c.zig` (`sqzhash_key_init`, `sqzhash128_keyed`, `sqzhash128`, `sqzhash64`). README §sqzhash. No external reference exists. Requires Zig 0.17.
- The repo has no git history, so it was copied into squeezefs-zig with provenance noted rather than vendored.
- API: `Key.init(secret: u128)`, `hash(key: *const Key, input) u128`, `hashSeeded(seed: u64, input) u128`, streaming `Hasher` with `init(seed)` / `initWithKey(key)` / `update` / `final() u128`.
- Uses `std.crypto.core.aes` with an aarch64 AES fast path gated on `builtin.cpu.has(.aarch64, .aes)`.
- Expand a `Key` once (e.g. per mount) and reuse it; `hashSeeded` expands a key per call and costs several ns more.

## Properties
- Keyed, non-cryptographic, 128-bit, AES-round based. Passes all 252 SMHasher3 `--extra` tests as 128-bit and on its low 64 bits (verification 0xAB49D6E0 and 0xD6703A98); truncating to 64 bits is sound.
- Inputs ≤ 16 B do a page-safe over-read like GxHash (can trap under Arm MTE). 17–256 B use three AES rounds per block to resist crafted collisions (covers NAME_MAX). Above 256 B lanes use one round per block — intended for checksums of non-adversarial data.
- AArch64 uses AESE/AESMC; other targets use std.crypto's AES round (AES-NI on x86_64 if enabled, else software). Only AArch64 was benchmarked; x86_64 correctness tested under Rosetta.
- Not for content identity/dedup — use BLAKE3 there.

## Adoption in squeezefs-zig (landed on dev at d93d499, format v5)
- Owner directive 2026-10-11: replace every xxHash128/xxHash64 use with sqzhash128; format bumped v4→v5 and mount refuses v4 loud. Decision 0003 §M, 0002 row 480, charter §5.1, evidence `docs/evidence/rung-09-sqzhash.md`.
- Port: `src/core/sqzhash.zig` — the pinned definition (28 format vectors, hardware/portable AES path equality via `Engine(comptime path)`, streaming ≡ one-shot).
- `src/core/checksum.zig` is the algorithm-neutral facade and the ONE hash site: `hash64` (= low 64 bits of the u128; 8-byte fields keep their width, no struct widens), `hash128`, `hash64Keyed(key, bytes)`, `hash64FieldZeroed`, `stamp`/`verify`, `Key`, streaming `Hasher` (`final`/`finalLow64`).
- `FORMAT_KEY = 0x5351_5A46_5320_464F_524D_4154_204B_4559` ("SQZFS FORMAT KEY", public, comptime-expanded) is used for all checksums (the superblock must verify before any per-volume value is known).
- Name hashing (hash54/hash56) uses `key.NameKey` = `Key.init(hash_seed)` zero-extended (`keyFromSeed`), expanded once per open, passed by pointer.
- Fields renamed `xxh3_le→csum_le`, `XXH3_OFF→CSUM_OFF`. Also bumped: data superblock v1→v2, framed image FRAME_VERSION 2→3, staging record 2→3 / marker 1→2, read-cache frame 1→2; fixture `tests/fixtures/findvsa_pred_bset.v5.bin`.
- `std.hash.XxHash3` survives only in the bench's reference column.
- Measured (venue, ReleaseSafe): 16 B 2.17→1.35 ns; 100 B 8.55→3.11 ns; 4 KiB 3.67 µs→29.4 ns; 256 KiB 207→2.60 µs. `std.hash.XxHash3` scalarizes its vector multiply on NEON — never estimate std's xxh3 speed from the C library.

## Staged cell streaming Hasher (landed d390207)
- The staged cell's `Hasher` was bounded from 4,736 B to 1,056 B per live cell (before: copied the 320 B expanded Key and held a 4 KiB pending buffer; worst case 139,777 cells × 4,736 B = 662 MB = 116 % of the ring, uncounted).
- Shape: `key: *const Key`, `total_len`, `union(enum){ prefix: [1024]u8, lanes: {[16]Vec, window: [256]u8} }` — inputs ≤ 1 KiB (`medium_max`) use shapes not derivable from the 16-lane state so the whole prefix is retained; beyond that 16 lane states + last 256 B. Size pinned by comptime assert + tie test; output unchanged (all vectors pass).
- Gauged as budget component `staging_cell_state` (`stats.staging_cell_state_bytes`, `Segment.cellStateBound(cap)`); rail `@sizeOf(ValueHash) < RECORD_ALIGN`.

## Open
- Verify deterministic output across aarch64 and x86_64.