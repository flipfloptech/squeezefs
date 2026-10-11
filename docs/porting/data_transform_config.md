# PORTING SPEC — data transform (lz4/zstd + AEAD), key handling, and the volume-set config

> **Status.** Research spec for rung 4 (the data plane). The reference is EVIDENCE (decision 0002):
> every byte table below is what `~/Source/squeezefs` writes, quoted `file:line`, read for its
> derivations and the WHY of each number. Every "**v4:**" row is what `squeezefs-zig` writes; no
> compatibility with the reference's images or wire is owed (0002 §2). §8 lists the quirks met here
> as ledger-row candidates for 0002.
>
> **Charter sources read in full:** §6.5 (`docs/charter.md:236-238`), §6.2 item 3 (`:212`), §8
> (`:248-256`), §5.4 (`:137-143`), §1.5–1.8 (`:24-27`), §5.1 superblock (`:104`). **Port state read:**
> `src/meta/superblock.zig:40-161` (192 B v4 superblock, four capability bits incl.
> `volume_lifecycle = 1<<3` at `:155-156`), `src/meta/ledger.zig:17-47,229-280` (membership stamp on
> every ledger record), `src/meta/geometry.zig:71-92`, `src/meta/format.zig:46-67` (`FormatOptions`),
> `src/meta/backend.zig:223` (`formatVolume` — one metadata volume), `src/cli/format.zig:81-166`
> (`sqdata://` URIs accepted, "NOT formatted or recorded", `:160-163`), `src/meta/key.zig:30-53`
> (`TREE_CONTROL = 8`, `TREE_DATA_ALLOC_RESERVED = 5`), `src/core/knobs.zig:57-84` (knob classes).

---

## 0. One-paragraph shape

A volume set declares, at `format`, a per-set **transform** (compression ∈ {none, lz4, zstd};
encryption ∈ {none, AES-256-GCM, ChaCha20-Poly1305}) and a **volume-set config** (block size,
capacity, the data volumes, the staging/cache paths, the key reference). The reference stores the
config as a **JSON xattr `user.squeezefs.format_config` on ino 1** of the slot-0 host
(`builder.rs:59`, `config_ops.rs:21-40`), rewritten whole by every admin verb through `setxattr`
(`config_ops.rs:591-606`, `:1448-1458`). Every non-passthrough block image is a self-delimiting
**frame** `[u32 LE len | raw<<31] ‖ image` (`crypto_compress.rs:9-19,34-47`), the image being
`AEAD-header ‖ ciphertext ‖ tag` when encrypted (`:49-53,708-718`). The data key is a per-process
random 32 B session key wrapped under a KEK derived by HKDF-SHA-256 from an operator key FILE and a
per-volume salt (`keyfile.rs:225-245`); the 63 B wrap blob rides every block header (`:65-73`).
A DATA volume carries **no identity whatsoever**: `format` zeroes `min(capacity, 32 MiB)` of it
(`main.rs:4523-4563`) and the mount opens it by PATH (`routing.rs:2936-2964`). v4 changes that (§5.6).

---

## 1. The transformed block frame — byte-exact

### 1.1 Reference layout (`crypto_compress.rs`)

Passthrough states (`compression == none ∧ encrypt == none`, `:468-471`) store the payload
**byte-identical with NO frame** — "the passthrough contract — §5.6 ranged reads depend on it"
(`:15-16`, `:936-940`, test `:1163-1171` asserts the same pointer).

Every other image, written by `process_write` (`:936-980`) or `process_write_pooled` (`:849-934`):

```
offset  len   field                                   source
0       4     frame word  u32 LE = image_len | FRAME_RAW_FLAG?   :19 FRAME_LEN_BYTES, :44 FRAME_RAW_FLAG = 1<<31,
                                                                 :47 FRAME_LEN_MASK = 0x7FFF_FFFF
4       …     image (image_len bytes):
              — compression-only:  compressed image | raw payload (raw iff bit 31)
              — encrypted:         AEAD envelope (below), over compressed|raw plaintext   :1018-1035
```

AEAD envelope (`encrypt`, `:688-719`; identical bytes from the pooled leg, `:864-889`, pinned by
`test_encrypted_scratch_header_parity` `:1673-1738`):

```
offset  len   field
0       2     wrapped_key_len   u16 BIG-endian (`(len>>8) as u8, (len&0xFF) as u8`, :711-712)  — always 63
2       1     nonce_len         u8 = 12 (:713; NONCE_LEN :53)
3       63    wrapped key blob  "SK" ‖ 0x02 ‖ nonce12 ‖ AEAD(KEK, aad=key_id, data_key32) ‖ tag16   (§3.1)
66      12    nonce             4 B process salt ‖ 8 B BE counter  (:677-686)
78      n     ciphertext        seal(plaintext), AAD EMPTY (:706, :882)
78+n    16    tag               AEAD_TAG_LEN_MAX = 16 for both algorithms (:78)
```

Reader (`process_read`, `:982-1036`): `len < 4` ⇒ refuse; `image = data[4 .. 4+image_len]` else
"malformed transform frame: claims N image bytes, M available — unframed legacy volume or corrupt
block" (`:1009-1017`); decrypt iff encrypted (`:1018-1022`); dispatch on bit 31: raw ⇒ the
(decrypted) bytes ARE the plaintext; else decompress (`:1024-1035`). Trailing device-window bytes
are ignored by construction — the reason the frame exists ("lz4's `decompress_size_prepended`
rejects trailing bytes with `OffsetZero`; AEAD opens `data[header..]`, so padding lands inside the
tag check", `:11-15`). Unframed legacy blobs refuse LOUD (`:16-18`; test `:1456-1459`).

**PERF-10 zero pad** (`:901-933`): the pooled image is zero-padded to the 4 KiB DMA grain iff the
padded length fits the scratch AND `≤ CHUNK_SIZE`; "costs zero device bytes and buys the aligned
(zero-copy) DMA branch"; instrument `nvme_unaligned_write_fallbacks → ~0 on transformed volumes`
(`fuse_client.rs:6207-6215`). The pad is ZEROS because recycled scratch bytes "must never reach the
device" (`:925-929`; tests `:1349-1369`).

### 1.2 Serde/library artifacts in the reference frame (flagged)

- lz4's own 4 B LE "prepend size" INSIDE the compressed image (`compress_prepend_size`, `:479`;
  reproduced by hand in the scratch leg `:788-795`) — a library framing nested in ours.
- `wrapped_key_len` big-endian beside a little-endian frame word (`:711-712` vs `:71`).
- Two length bytes (`wrapped_key_len`, `nonce_len`) that only ever hold 63 and 12 (`:73`, `:53`).
- zstd `encode_all` streaming frame (`:483`) vs `bulk::Compressor::compress_to_buffer` (`:797`) —
  two zstd frame shapes for one volume, both decoded by a streaming reader with a `take(cap+1)`
  bound (`:540-561`).

### 1.3 v4: ONE fixed 96 B header, canonical and total

```zig
/// Every transformed block image: header ‖ body ‖ zero pad to 4 KiB. Little-endian throughout.
pub const TransformFrameWire = extern struct {
    magic: [4]u8,            // "SQZB"
    version: u8,             // 1
    flags: u8,               // bit0 STORE_RAW, bit1 ENCRYPTED, bit2..7 must be 0
    compression: u8,         // 0 none, 1 lz4, 2 zstd  (the volume's declared mode; a mismatch refuses)
    encryption: u8,          // 0 none, 1 aes256gcm, 2 chacha20poly1305
    body_len: u32,           // bytes after the header: plain|compressed image, or ciphertext ‖ tag
    plain_len: u32,          // declared plaintext length ≤ block_size (the DUR-8e bound, checked FIRST)
    key_blob: [63]u8,        // "SK" ‖ 0x02 ‖ nonce12 ‖ ct32 ‖ tag16; all-zero when !ENCRYPTED
    nonce: [12]u8,           // all-zero when !ENCRYPTED
    reserved: [5]u8,         // 0
};
comptime std.debug.assert(@sizeOf(TransformFrameWire) == 96);
```

- **AAD = header[0..96]** (the whole header). The reference seals with EMPTY AAD (`:706,:882`): a
  flipped `raw` bit or a swapped `plain_len` is undetected until the decompressor chokes. Binding to
  the block's LOCATION is deliberately NOT done: the defrag/drain mover copies images byte-wise and
  remote job workers are keyless (charter §8 "100 % verify-reads before publish on plaintext").
- `plain_len` replaces lz4's nested prepend-size; lz4 block decode takes the exact destination size
  (no size-prepended variant), zstd decodes into a `plain_len` destination and refuses a frame that
  disagrees. One zstd shape: single-shot (`ZSTD_compress2`), content-size known.
- Total: every field is read by position; `magic`, `version`, zero `reserved`, zero `flags` bits,
  `compression/encryption == the volume's` are each checked and refused loud by name; `body_len`
  must fit the device window; `plain_len ≤ block_size` before any allocation.
- Passthrough volumes keep the reference's NO-frame contract (ranged reads, W1, zc bridges).
- `max_stored_image_len(block) = 96 + block` (§4); the 4 KiB pad law is kept.

### 1.4 Version 2 (2026-10-11, decision 0003 §K7): the FRAMED image — `header ‖ table ‖ frames`

One frame per block made every sub-block read decode the whole image (1025× device bytes for a
4 KiB read; measured 446× on the venue). Version 2 splits the plaintext into `F = 64 KiB` frames
(`transform.FRAME_GRAIN`: lz4's match window, so no ratio is lost; zstd −3 loses a few percent at
64 KiB; AES-GCM pays 28 B per frame) and reads only the frames a window needs. Version 1 images
are refused (`transform_frame_refusals_version`; pre-release, decision 0002).

```
header   96 B   TransformFrameWire as §1.3 with version = 2; flags: bit1 ENCRYPTED only (STORE_RAW
                moved to the entry); body_len = Σ frame bodies AFTER the table; nonce = the block's
                BASE counter (frame i seals under base + i; the writer mints `frames` counters at once)
table    16 B   FrameTableWire { entries_xxh3 u64 (over the entries), frame_count u16, frame_grain_log2 u8 = 16, reserved[5] }
entries  16 B × frame_count   FrameEntryWire { off u32 (from the image start; contiguous), body_len u32, plain_len u32 (F but the last), flags u8 (bit0 STORE_RAW), reserved[3] }
frames   frame 0's body begins at tableLen(count) = 96 + 16 + 16·count (128 B for one frame, 1,136 B for 64);
         each body = compressed-or-raw plaintext ‖ tag (encrypted); AAD = header ‖ entry ‖ index (u32 LE)
pad      zeros to the 4 KiB DMA grain
```

- **The table is compact, never a padded page.** The header ‖ table always fit the image's FIRST
  4 KiB grain (`MAX_FRAMES = 248 ≥ 64`), so a sub-block read DMAs that grain alone to learn its
  frames' window (`frameSpan`: `[floor_4k(first.off), ceil_4k(last.off + last.body_len))`), DMAs
  the window — reusing the grain in hand when frame 0 is inside it — and decodes those frames
  alone (`decodeFrames`; `read_path.zig` arm 8f, never published, ghost-escalated like a ranged
  read). A padded header page would have doubled every small packed tenant's slot
  (`pack_slot_len = ceil_4k(image)`) and the DMA that reads it.
- `max_stored_image_len(block) = tableLen(frames) + block + frames × 16 (encrypted)`; the headroom
  stays ONE grain (`1,136 + 1,024 ≤ 4,096`), `chunk = block + 4 KiB` as before.
- The data superblock's `features_incompat` bit 0 (`data_superblock.feature_framed_images`) names
  the format on transformed volumes: an older train refuses the bit loud at mount; this train
  refuses a transformed volume without it (another train's images — reformat).
- fsck's C-pass verifies a framed image through the same decoder: the table's xxh3, the bodies'
  coverage of `body_len`, every frame's tag or decode.
- Bound for a random 4 KiB read (decision 0003 §K7): `(4 KiB + ceil_4k(F + tag)) / 4 KiB ≤ 18×` on
  an incompressible frame, `(4 + ceil_4k(F/ρ)) / 4` at ratio ρ.

### 1.5 The codec contexts (2026-10-11, decision 0003 §K10): a codec per lane, a scratch per DMA

The reference's `scratch_pool_cap = clamp(cpus/4, 4, 16)` (measured: above the concurrent-transform
count every handout rotates through a cache-cold buffer, +4..10 %) is the stored-image SCRATCH pool;
its transform state is per call. The port had ONE `Transform` behind a mutex and ONE write scratch
— every lane's frame decode queued (849 IOPS on the zstd sub-block read), and the scratch was
overwritten by the next upload's transform while the previous upload's DMA still read it.

- `transform.Transform` — the volume's IMMUTABLE half: the modes, the key state (its own atomics),
  the raw lever. `transform.Codec` — the per-LANE half: a static `ZSTD_CCtx` + a static `ZSTD_DCtx`
  of the vendored library (its "one context per thread" contract; ≈ 96 KiB per DCtx, the CCtx by
  `ZSTD_estimateCCtxSize(3)`), present iff the volume compresses with zstd; lz4 and the AEADs are
  pure functions of their arguments. Every entry point (`processWrite`, `processRead`,
  `decodeFrames`) is the codec's.
- `transform.CodecPool`: `codecPoolSize(lanes) = max(lanes, 1)` codecs (every executor lane — the
  fuse handler lanes, the meta pool, the lease and main lanes — may transform; a transform never
  parks holding a codec, so one per lane thread is exact and `transform_codec_pool_waits` stays 0 by
  derivation); a Vyukov free list under a semaphore (the fast path one pop + one push; an acquirer
  past the capacity parks, counted, never allocates); a stateless volume hands out one shared codec
  with no atomics.
- `transform.ScratchPool`: `scratchPoolCap(cpus)` buffers of `scratchLen(block)`, DMA-aligned, each
  held from the transform THROUGH the DMA that reads the image (`transform_scratch_pool_waits`
  counts the parks past the cap). The plane owns one (`transformWrite` → `Stored{bytes, scratch}`
  released by the caller after its DMA: the own-block act, the pack prepare; `readMappingImage`'s
  decode destination); the write path owns one (`uploadParkedBlock`: acquired before the transform,
  released after `device.write` returns).
- `scratchLen(block) = writeScratchLen(block)` = block + 4 KiB: the C decoder writes exactly the
  content size (std's streaming decoder's window law — block + 128 KiB, the charter's envelope —
  is retired with it; decision 0002 row 118 → 477).
- Zstd decompression: `ZSTD_decompressDCtx(dctx, out, plain_len, src, len)`; `n != plain_len` or an
  error code ⇒ `Corrupt` — a frame declaring more content refuses on the bounded destination, one
  declaring less on the count, trailing bytes on the library's frame walk. Total over arbitrary
  input (the PRNG property test and the fuzz rows stand).

---

## 2. Compression and AEAD

### 2.1 Algorithms and parameters

| item | reference | WHY / v4 |
|---|---|---|
| lz4 | `lz4_flex` block format (`compress_prepend_size` `:479`; `block::compress_into` `:789`); decode `decompress_size_prepended` `:531` | fast arm; **v4:** raw lz4 block (vendored C `lz4` — std has no lz4), sizes from the header |
| zstd | level **3** (`:483`, `:797`); WHY: the library default `ZSTD_CLEVEL_DEFAULT` | **v4:** level 3 single-shot via the vendored `zstd` in a static `ZSTD_CCtx`; decode through the same library's static `ZSTD_DCtx` (§1.5; std's decoder was 10× slower and wanted a window-sized scratch) |
| AES-256-GCM | `ring::aead::AES_256_GCM` (`:4`, `:164`), 32 B key (`:71`), 12 B nonce, 16 B tag | **v4:** `std.crypto.aead.aes_gcm.Aes256Gcm` |
| ChaCha20-Poly1305 | `CHACHA20_POLY1305` (`:165`) | **v4:** `std.crypto.aead.chacha_poly.ChaCha20Poly1305` |
| mode spellings | `"none"|""`, `"lz4"`, `"zstd"` (`:126-129`); `"aes256gcm"|"aes256gcm-rsa"`, `"chacha20"|"chacha20-rsa"` — the `-rsa` names are deprecated aliases "retained only so a pre-KW-1 config still parses far enough to be refused" (`:114-116`, `:141-144`) | **v4:** u8 enums in the config record; no string parsing per block (the reference's own P2-3 "no string trim/match per block", `:105`, `:938`) |

### 2.2 Nonce construction

`next_nonce_bytes` (`:677-686`): `nonce[0..4] = salt` (4 random bytes drawn once per
`CryptoCompressState` at `:322-323`), `nonce[4..12] = counter.fetch_add(1, Relaxed).to_be_bytes()`.
The session DATA KEY is itself fresh per process (`:296-307`), so uniqueness is per `(key, process)`
— the salt adds nothing a fresh key does not already give. **v4:** the same 12 B shape (a per-mount
random 32-bit prefix + a 64-bit monotone counter per mount), the counter a `counters.Gauge`-visible
word; exhaustion (2^64) is unreachable and still a `Tripwire`.

### 2.3 The per-block best-effort rule and the raw bit (FIND-RW4-A)

`compress_or_raw_into_scratch` (`:820-834`) and the heap leg (`:954-963`): compress; **if
`n >= data.len()`** (equal counts too — "an equal-size image is pure decompress cost for zero gain",
`:813-815`) overwrite with the raw payload, set bit 31, count `compress_stored_raw`
(`fuse_client.rs:6173-6179`: "growth ≈ the incompressible share of the write mix; a compressible
workload must keep this ~0"). The escape sits BELOW the AEAD layer: encrypted volumes store
`encrypt(raw payload)` with the marker (`:38-39`; test `crypto_incompressible_tests.rs:27-30`).
The empty payload is stored raw (lz4's 4 B prefix "cannot shrink 0 bytes", `:1412-1418`).
Compression is thereby "best-effort per block (btrfs/zfs practice)" (`crypto_incompressible_tests.rs:25`).

### 2.4 Read-side bounds (DUR-8e, `:492-565`)

`max_plaintext_len = configured block size` recorded by `init_scratch_pool` (`:440-445`), falling
back to `routing::default_block_size()` (`:502-505`; `SQUEEZEFS_DEFAULT_BLOCK_SIZE` or 4 MiB,
`routing.rs:7706-7711`). lz4: refuse `declared > cap` BEFORE the allocation `decompress_size_prepended`
would make from the on-disk length (`:514-535`). zstd: streaming reader `take(cap+1)`, `read > cap`
⇒ refuse (`:536-561`). Test `:1570-1602` pins "a plaintext above the configured block size must be
refused, not allocated". **v4:** `plain_len ≤ block_size` is the one check, on the header, before
anything else; the decompressor writes into a pooled `block_size` destination, never allocates.

### 2.5 The scratch pool (§5.7 of the reference's design)

- `worst_case_scratch_len(n) = 4 + max(4 + lz4_bound(n), zstd_bound(n)) + encrypt_overhead`
  (`:350-355`, `:378-380`), `encrypt_overhead = 3 + wrapped_len(63 | 512 fallback) + 12 + tag`
  (`:361-371`); buffer = `next_multiple_of(4096)` (`:449-451`). Library bounds (verify at port
  time): `lz4_bound(n) = 16 + n + n/255`; `zstd_bound(n) = n + (n>>8) + (n < 128 KiB ? (128 KiB − n)>>11 : 0)`.
  At 4 MiB blocks: lz4 term 4,210,772 > zstd 4,210,688; + 98 ⇒ 4,210,870 → **4,214,784 B = block +
  20 KiB**; at 256 KiB: 266,240 = block + 4 KiB. The charter's "block + 128 KiB" (§6.2 item 3) is the
  envelope the test asserts (`buf_len ≤ bs + 128 KiB`, `:1308`).
- Capacity `(cores / 4).clamp(4, 16)` (`:30-32`); WHY: FIFO rotation through more buffers than the
  concurrent-transform count is cache-cold, "measured +4..10 % on the 4 MiB lz4/aes micro-benches at
  cores × 4 buffers" (`:21-29`); `cores` is the fleet-share-divided root (`:28-29`, `:452-456`).
- Exactly ONE pooled buffer per transform: compress into the scratch at the sealed-payload offset,
  `seal_in_place_separate_tag` at the offset, tag after (`:836-848`); the input is never mutated
  (`:1625-1643`); 4096-aligned backing (`:1512-1514`); overflow past the pool's basis bounces to the
  heap (`:946-950`, test `:1570-1602`).
- **v4:** the scratch is the write-through path's pooled `block + 4 KiB` buffer sized from the ONE
  header (`96 + max(lz4_bound, zstd_bound)` rounded up); capacity derives from the write-pipeline
  depth governor's in-flight count (charter §6.2), floor 4 (the shipped posture), never a heap
  fallback on a product path (charter §12).
- `process_write_async` offloads ≥ 64 KiB to `spawn_blocking` (`:1038-1050`) — a tokio artifact;
  **v4:** the transform runs inline on the handler lane's fiber (CPU work on the lane that owns the
  block; ~1–2 ms per 4 MiB lz4+AES).

---

## 3. Key handling (`keyfile.rs`, KW-1 / VAL-3 / VAL-7h)

### 3.1 The wrap `hkdf-sha256/aead-kw/v2` — every string and length

| element | value | source |
|---|---|---|
| scheme id | `"hkdf-sha256/aead-kw/v2"` | `keyfile.rs:28` |
| material | key file bytes, ASCII-whitespace trimmed both ends, **≥ 32 B** (`MIN_KEY_MATERIAL_BYTES`), ≤ 64 KiB (`MAX_KEY_FILE_BYTES`) | `:36-43`, `:91-118` |
| salt | 32 B (`KDF_SALT_LEN`, "RFC 5869 recommends the hash length"), fresh per `format` from `SystemRandom` | `:45-46`, `:208-222` |
| PRK | `HKDF-Extract(salt, material)` (SHA-256) | `:232` |
| KEK | `Expand(PRK, info = "squeezefs:key-wrap:v2:kek", L = 32)` | `:55`, `:233-238`, `KEK_LEN :49` |
| key_id | `Expand(PRK, info = "squeezefs:key-wrap:v2:key-id", L = 8)` | `:56`, `:239-243`, `KEY_ID_LEN :53` |
| wrap AEAD | the volume's OWN record algorithm over the KEK ("one primitive per volume", `crypto_compress.rs:279-292`) | |
| blob | `"SK"(2) ‖ 0x02(1) ‖ nonce(12, random per wrap) ‖ seal(KEK, nonce, aad = key_id(8), data_key(32)) ‖ tag(16)` = **63 B**, fixed-width | `crypto_compress.rs:65-73`, `:239-268`, test `encrypt_key_handling_tests.rs:317-328` |
| unwrap refusals | `len < 15 ∨ magic ≠ "SK"` ⇒ "malformed key-wrap blob"; `blob[2] ≠ 2` ⇒ "unsupported key-wrap version N"; AEAD failure ⇒ "written under a different encryption key (or the header is corrupt)"; `plain.len ≠ 32` ⇒ refuse; never panics on attacker bytes | `:580-631`, test `:331-365` |
| what is persisted | `EncryptKeyRef { scheme, kdf_salt (hex 64 chars), key_id (hex 16 chars) }` — "three public fields" | `keyfile.rs:172-180`, `:248-254`, test `:264-285` |
| session key | 32 B random per `CryptoCompressState::new`; pre-wrapped once; the blob + precomputed `LessSafeKey` reused on every write (`resolve_encrypt_key`) | `crypto_compress.rs:295-317`, `:638-650`, test `:1137-1160` |
| unwrap cache | moka, `max_capacity(10000)`, keyed by the blob bytes; `key_unwrap_count` | `:326`, `:740-761`, test `:1102-1134` |

**v4:** identical derivation strings and lengths (they are the charter's §6.5 text), through
`std.crypto.kdf.hkdf.HkdfSha256` and the volume's `std.crypto.aead` type. The key ref in the
config record is binary: `scheme u8 = 2, kdf_salt [32]u8, key_id [8]u8` (no hex). Unwrap cache: a
fixed-capacity table owned by the mount's crypto state (capacity derived from
`max(16, writers_known × 2)`, floor 16 — one blob per writer-process incarnation that still has
live blocks), counters `crypto_key_unwraps`, `crypto_key_unwrap_hits`.

### 3.2 Key resolution order (`resolve_volume_key`, `:436-480`)

1. `--encrypt-key <path|->` material stashed in the PARENT before `--daemon` forks so stdin works
   (`main.rs:2871-2890`; `keyfile.rs:409-424`);
2. `SQUEEZEFS_ENCRYPT_KEY_FILE=<path>` (`:31`, `:445`, `:450-452`);
3. `/etc/squeezefs/keys/<key_id>.key` (`KEY_DIR :34`, `default_key_path :428-430`, `:453-459`);
4. none ⇒ refuse naming all three sources (`:460-467`).

Then **`derive(material, salt).key_id` must equal the recorded `key_id`** — "a wrong key file is a
loud refusal at mount instead of an AEAD failure on every read" (`:469-479`). An unknown scheme
refuses "upgrade squeezefs" (`:437-443`). `--encrypt-key` on a plaintext volume refuses
(`mount_volume_key`, `:532-541`); `format --encrypt-key` without an algo refuses, and an encrypting
algo without a key refuses (`main.rs:4139-4171`). The mount preflights the key in the CLI parent
(`main.rs:3432-3441`) AND re-resolves at FUSE init (`fuse_client.rs:26580-26595`) — "a mount reached
by any other route (tests, embedders) still gets the gate".

### 3.3 Key file laws (`read_key_file`, `:330-403`) — validated on the FD, no TOCTOU

- `open(O_RDONLY | O_NOFOLLOW | O_CLOEXEC)`; `ELOOP` ⇒ "is a symlink — refusing to follow it" (`:341-356`);
- `fstat`: `S_IFREG` only (`:368-371`); `mode & 0o077 == 0` else "readable or writable by group/other … `chmod 600`" (`:372-379`);
- owner: `euid != 0 ⇒ st_uid == euid` else refuse (`:380-388`) — root accepts ANY owner (quirk §8.13; the charter says "owned by euid/root");
- `st_size ≤ 64 KiB` (`:389-395`); read into `Zeroizing<Vec>`; trim; `≥ 32 B` (`:397-402`).
- stdin (`-`): `take(MAX+1)`, `> MAX` ⇒ "that is not a key file" (`:315-327`).

### 3.4 Process hardening and zeroize (VAL-7h)

`harden_process_memory` (`:270-297`): `prctl(PR_SET_DUMPABLE, 0)` + `setrlimit(RLIMIT_CORE, {0,0})`
on EVERY key-touching call (never latched — "a forked child whose flag was cleared"), the info line
once per process; failures WARN ("a core dump of this daemon could spill encryption key
material"). Called from `read_key_reader` (`:316`) and `read_key_file` (`:335`) — so only on paths
that actually touch a key, "never on plaintext mounts" (`:264-266`). Test re-execs itself and
asserts `PR_GET_DUMPABLE == 0` and `RLIMIT_CORE == (0,0)` (`encrypt_key_handling_tests.rs:575-616`).
Zeroize: `KeyMaterial(Zeroizing<Vec>)` (`:89`), `VolumeKey.kek: Zeroizing<[u8;32]>` (`:136`), the
session key `Zeroizing<[u8;32]>` (`crypto_compress.rs:186`), unwrap scratch zeroized on both exits
(`:622,:629`); `Debug` redacts everywhere (`keyfile.rs:74-78,125-129,155-163`;
`crypto_compress.rs:215-225` — "a manual `Debug` also stops a future `#[derive(Debug)]`").
**v4:** `std.crypto.secureZero` on every key-bearing buffer's scope exit (`defer`); key types have no
`format` method; `prctl`/`setrlimit` through `std.os.linux` at the first key read.

---

## 4. `max_stored_image_len(block)`, the headroom, the geometry refusal

- `max_stored_image_len(payload) = passthrough ? payload : 4 + encrypt_overhead + payload` with
  `encrypt_overhead = 3 + 512 + 12 + 16 = 543` (`crypto_compress.rs:391-401`) — the raw escape makes
  the compression term never exceed the payload (`:382-385`). The 512 is `WRAPPED_KEY_LEN_FALLBACK`,
  "the retired RSA-4096 value" kept because "shrinking it moves the clamp, i.e. it is an on-disk
  change" (`:55-63`). Readers size their device window with it: `device_block_window =
  ceil_4k(max_stored_image_len(bs)).min(CHUNK)` (`routing.rs:12145-12153`).
- `TRANSFORM_BLOCK_HEADROOM = 4096` (`:80-91`): the worst-case non-payload bytes (4 + 3 + 512 + 12 +
  16 = 547) rounded to one LBA "so chunk-interior windows stay O_DIRECT-aligned"; a const assert
  pins `headroom ≥ 547` (`:93-103`).
- `format` **clamps `block_size` to `CHUNK_SIZE − 4096`** on transformed volumes, printing the clamp
  (`main.rs:4316-4339`); `CHUNK_SIZE = 4 MiB` is the allocator stride (`block_allocator.rs:106-112`);
  `--block-size > CHUNK` refuses (`main.rs:4306-4315`).
- `transform_geometry_check(block, chunk)` refuses the mount iff `max_stored_image_len(block) >
  chunk` on a transformed volume, with the operator text "…formatted before the incompressible-block
  fix; its full-size incompressible blocks were never readable. Reformat…" (`:403-432`); run in the
  CLI parent (`main.rs:3466-3482`) and at FUSE init (`fuse_client.rs:26601-26614`); tests
  `crypto_incompressible_tests.rs:616-673`. `ensure_stored_block_image_fits` is the write-path belt:
  `stored_len > chunk` ⇒ loud EIO, "never truncates or overflows" (`block_allocator.rs:114-130`).

**v4 decision (chunk vs block):** the reference shrinks the LOGICAL block to `4 MiB − 4 KiB =
4,190,208 B` (not a power of two; every block boundary leaves the 1 MiB writeback grid of charter
§6.2, so every kernel piece straddles). Better: the block stays the power of two the operator named
and the allocator stride grows — `chunk = block + headroom` on transformed volumes (4,198,400 B =
1025 LBAs), `chunk = block` on passthrough; `headroom = ceil_4k(@sizeOf(TransformFrameWire)) = 4096`
(the 96 B header; the 512 B RSA ghost is gone). The refusal law stays: a config whose
`chunk < max_stored_image_len(block) = 96 + block` refuses the mount by name. Capacity in blocks =
`floor((volume_len − 2 × chunk) / chunk)` (§5.6 reserves the first and last chunk).

---

## 5. THE VOLUME-SET CONFIG

### 5.1 Reference: location and codec

- **Location:** xattr `user.squeezefs.format_config` on ino 1 (`builder.rs:59`), written by the first
  volume's format (`main.rs:4440-4447`, `kv_alloc_superblock.md` §2.3 step 8), READ via
  `slot0_root_ino()` of the slot-0 host after a migration (`config_ops.rs:24-27`), and the host is
  "routing slot 0 in CANONICAL set order" (`config_home_volume`, `:513-526`). fsck REQUIRES it
  (0002 row 8). The record is screened from FUSE by the `user.squeezefs.` reservation and written
  through `setxattr_internal` (`:595-606`).
- **Codec:** `serde_json` (`:35-39`, `:592`, `:1448`) — **a serde artifact as on-disk format**
  (0002 row 2's class): optional fields `skip_serializing_if`, additive decoding, no checksum, no
  version, rewritten whole by every verb.
- **Schema** (`lib.rs:1351-1421`): `name: String ("squeezefs")`, `block_size u64`, `capacity u64`,
  `inodes u64`, `compression: String`, `encrypt_algo: String`, `encrypt_key: Option<RedactedSecret>`
  (legacy cleartext PEM — read-only, `skip_serializing`, `:1359-1367`), `encrypt_key_ref:
  Option<EncryptKeyRef>`, `mem_cache_size / disk_cache_size / read_cache_size / write_cache_size /
  read_mem_cache_size / write_mem_cache_size: Option<String>` ("1GB", "10GB", "50 %" splits,
  `main.rs:49-66`), `disk_cache_paths: Option<Vec<PathBuf>>`, `data_lv: Option<Vec<String>>` (the
  legacy path list, MIRRORED on every write `config_ops.rs:1440-1446`), `data_volumes:
  Option<Vec<DataVolumeRecord>>` (None on a fresh format — "legacy-shaped … materialize only at the
  first lifecycle verb", `main.rs:4356-4360`), `dismount_wait`, `upload_delay ("500ms")`,
  `fuse_io_uring_sqpoll_idle_ms`, `meta_routing_width` / `meta_slot_runs` / `meta_volumes` — all
  three "MIRROR ONLY — the stamps are authoritative" (`lib.rs:1405-1421`).
- `DataVolumeRecord { id: "vol-{16 hex}", backing_dev: String, state: String, added_ts: u64 }`
  (`lib.rs:1028-1042`); id minted `format!("vol-{:016x}", fastrand::u64(..))` — "random, allocated
  once, never reused (KD-5 — retired ids are recorded forever so a stale key can never resolve to
  the wrong device)" (`:1044-1049`); legacy ids = device basename (`legacy_volume_id`, `:1342-1349`;
  `resolved_data_volumes`, `:1429-1445`). `vol_tag(id)` = the 16 hex decoded verbatim as u64, else
  `xxh3_64(id)` for basenames (`block_refs.rs:181-202`) — "the tag is a KEY component … identical on
  every mount forever … never from a path, a mount-order ordinal, or a set position".
- States: `active | disabled | draining | retired` (`lib.rs:1008-1020`): `Active → Draining →
  Retired`, `Draining → Active` on `undrain`; `Retired` terminal, record kept FOREVER; `disabled` =
  the fail-stop health override (reads EIO, no placements, "NOT an evacuation",
  `routing.rs:3155-3177`); `placement_eligible = healthy ∧ state == active` (`:3209-3221`).

### 5.2 Reference: every verb, its mutation and durability order

Common guard: `format_preflight(path, force = true)` on EVERY meta volume = the live-client gate
("a volume any client has LIVE-mounted refuses", `config_ops.rs:539-543`, `:575-578`, `:925-932`);
then `read_volume_format_config` (fails loud on unformatted). Durability: `commit_volume_records`
(`:1406-1474`) = routed open of the whole set (D0 claims) → **`VOLUME_LIFECYCLE` bit 3 stamped on
every member superblock BEFORE the record** ("bit-before-durable-record … a crash after the bit and
before the record leaves a set old binaries refuse and this binary mounts unchanged", `:1425-1438`,
`superblock.rs:1557-1570`) → ONE `setxattr` tx of the whole JSON (+ rider xattrs) → clean shutdown
(the final checkpoint makes it durable).

| verb | mutation | source |
|---|---|---|
| `config get-cache-paths` | read `disk_cache_paths` | `:530-533` |
| `config set-cache-paths <paths…>` | live gate on every volume → config must exist → ALL new dirs precheck (ENG-4 denylist `/ /home /etc /usr /var /boot /root`, `:112-113`; non-empty unmarked dirs need `--force/--yes`) before ANY is wiped → wipe + recreate + chown to the invoking owner (`SUDO_UID:SUDO_GID` under sudo, `:70-100`), 0700 dirs / 0600 files (`:130-133`) → `cfg.disk_cache_paths = paths` → one setxattr → shutdown. "Content safety does not depend on the wipe: staging generation-binding discards foreign content at mount anyway" | `:534-609` |
| `volume add-data <dev>` (offline) | live gate → `validate_new_data_volume` (exists/sized/not a member by canonical path/not a meta volume, `:818-848`) → `probe_data_volume_rw` (random 4 KiB at offset 0, read back, zero-restored, `:850-894`) → record `{vol-…, dev, active, now}` → `commit_volume_records` + a durable Queued `rebalance` job (throttle 25 %) unless `--no-rebalance` | `:896-961` |
| `volume add-data` (online, admin lane) | same preflight on the live config → build backend (placement NOT enabled) → bit 3 on every member → setxattr through the live conveyor → `publish_backend` LAST ("the new capacity participates only after the next mount is guaranteed to know the id") | `fuse_client.rs:10281-10380` |
| `volume enable/disable <id>` | flip `state ∈ {active, disabled}`; idempotent no-op never materializes records | `config_ops.rs:963-1001` |
| `volume remove-data <id>` (offline) | live gate → victim must be `active|draining` → routed open → in-process router + fabric → **capacity preflight** (§5.5) → bit 3 → `Active → Draining` one tx → evacuation job to completion (`PausedCapacity` ⇒ loud refusal) → reclaim drain BEFORE the claims drop → `Retired` | `:1085-1320` |
| `volume undrain <id>` | `draining → active`, cancel every non-terminal `EvacuateVolume` job record in the same open; retired refuses ("ids are permanent") | `:1003-1083` |
| `volume list` | offline: records + `device_capacity_bytes`; used/free "omitted rather than guessed" | `main.rs:5082-5138` |
| `volume repair-set` | re-stamp a COHERENT observed membership; refuse everything else | `config_ops.rs:1476-1510` |

### 5.3 Reference: the generation stamp and the staging drain on SET changes

- `volume_set_generation(meta) = discover → generation_from_uuids(ordered uuids)` =
  `"v3:" ‖ hex(uuid)` joined by `|` (`meta_backend/mod.rs:6691-6713`); the staging generation
  decorates it with the writer scope `…"‖node:"{node:016x}["‖slot:"{slot:08x}]`
  (`writer_scope.rs:720-729`); marker file `.squeezefs_generation` per staging root
  (`cache/nvme.rs:15`).
- META set changes (`add-meta`/`remove-meta`) run KD-8: `staging_rebind_prepare` BEFORE any stamp
  flips — refuse loud on pending write custody (`active_block:`/`active_block_ext:` records: "mount
  the filesystem, let writeback drain (sync + clean unmount), then re-run; nothing was changed") and
  write a two-phase dual marker `(old, new)` on every old-generation root (`config_ops.rs:1815-1869`);
  `staging_rebind_finalize` AFTER the flip rewrites the single new marker (`:1871-1895`); counter
  `staging_drain_barriers` (`fuse_client.rs:7485`). DATA set changes do NOT rebind (the generation
  is a function of the META uuids only).
- The config itself carries NO generation: last-writer JSON.

### 5.4 Reference: discovery and refusals (`discover_meta_set`, `mod.rs:6316-6527`)

Probe every URI's sector 0 + 128 KiB ledger WITHOUT mounting (`observe_meta_set`, `:6257-6292`);
then, each a loud refusal naming the volumes: all stampless ("torn or foreign; reformat required",
`:6334-6350`); mixed stamped/stampless (`repair-set`, `:6351-6365`); a tombstone `member_count == 0`
("RETIRED member — remove it from the URI", `:6376-6385`); different `set_uuid` ("belong to
DIFFERENT sets", `:6389-6398`); `routing_width` disagreement (`:6399-6408`); `member_count`
disagreement ("an interrupted add-meta/remove-meta … re-run … idempotent", `:6409-6420`); stamps
declare N members but the URI lists M ("a member is missing from the URI" / "listed that the stamps
do not name", `:6422-6435`); duplicate `member_position` (`:6440-6454`); slot ≥ W (`:6462-6469`);
same-epoch dual claim ("impossible by construction … treat as corruption", `:6473-6481`); higher
epoch wins a dual claim (`:6482-6488`); a slot hosted by nobody (`:6493-6502`). Canonical order =
ascending `member_position` (sparse after remove-meta, `:6437-6443`). The port already has the stamp
(`ledger.zig:233-280`) minus `routing_width` (derived, 0002 row 41).

### 5.5 Reference: capacity preflight (`jobs.rs`)

`refuse iff avail < needed + transient + headroom` (`:423-450`): `needed` = deduped census of
referenced bytes on the victim (a clone-shared block counts ONCE, `:429-431`, `:1066-1068`);
`avail` = Σ free over healthy `active` survivors (`:432-433`); `transient = 64 blocks × ACTUAL
workers × block` (`EVACUATE_INFLIGHT_WINDOW_BLOCKS = 64`, "an in-flight moved block occupies source
AND destination until the post-publish free", `:111-115`, `:468-472`); `headroom =
max(write_rate_est × drain_eta, 1 GiB)` (`DRAIN_HEADROOM_FLOOR_BYTES = 1<<30`, `:117-119`,
`:475-479`), initial `eta = needed / (512 MiB/s) + 1` ("½ of a ~1 GiB/s devsub-class raw copy",
`DRAIN_FLOOR_RATE_BYTES_PER_SEC`, `:121-125`, `:1071`), re-verified at every coordinator checkpoint
with the measured job rate (`:2820-2850`). A refusal prints every term ("honest refusal", `:453-463`).
Property test `volume_drain_tests.rs:356-404`. `format --capacity > Σ physical` refuses ("oversubscription
is not supported … thin-provision underneath", `main.rs:3762-3782`).

### 5.6 v4: what rung 4 RECORDS and WRITES

**(a) Location: ONE control record on volume 0's tree 0** (`TREE_CONTROL = 8`, `key.zig:48` — the
control tree already exists empty at format, `geometry.zig:9-13`). Key `0x08 ‖ "set_config"` (the
charter's `slot_state:{s}` naming style, §5.5). It is a FORMAT-time record (written by the format
tx, so a fresh set is never "legacy-shaped"), mutated only by admin verbs under the D0 guard, and
the ONE source of the data-volume set; the membership stamp remains the META set's one truth (0002
row 46). No xattr, no `user.*` name, no fsck dependency on it beyond "present and decodable".

**(b) Codec: canonical, total, versioned, checksummed, every field by position**
(the `ledger.zig:17-33` discipline), little-endian:

```
VolumeSetConfigWire (fixed head, extern struct, 128 B):
  magic[8] "SQZSETCF" ‖ version u16 = 1 ‖ reserved u16 = 0 ‖
  config_generation u64        (monotone; +1 per committed mutation; the live mirror's compare word)
  block_size u32 ‖ chunk_size u32           (chunk = block + headroom(transform); §4)
  capacity u64                 (formatted capacity ≤ Σ physical at format; §5.5)
  compression u8 ‖ encryption u8 ‖ key_scheme u8 (0 none, 2 = hkdf-sha256/aead-kw/v2) ‖ reserved u8
  kdf_salt[32] ‖ key_id[8]     (zero when encryption == 0)
  n_data u16 ‖ n_cache_paths u16 ‖ reserved[4]
  reserved[32] = 0
then n_data × DataVolumeRecordWire (32 B fixed + uri):
  vol_tag u64 ‖ state u8 (1 active, 2 disabled, 3 draining, 4 retired) ‖ reserved[7] ‖
  added_ts u64 ‖ capacity_at_add u64 ‖ uri_len u16 ‖ reserved u16 ‖ uri[uri_len] (absolute path, no NUL)
then n_cache_paths × { len u16 ‖ path[len] }
then xxh3_64 u64 over everything before it.
```

- `vol_tag` IS the id (`vol-{16 hex}` is its spelling; the reference already decodes it verbatim,
  `block_refs.rs:193-199`); minted by `Io.randomSecure`, never reused: a `retired` record stays
  forever and `validate_new_data_volume` refuses a `vol_tag` or a device identity already present.
- A retired record keeps `uri_len = 0`.
- The record cap: `n_data ≤ 4096`, `uri_len ≤ 4096`, total ≤ the per-volume record cap
  `recordValueCap(node_size)` (`geometry.zig:139`) — refused at the verb, never at mount.
- Deleted from the schema (constants the charter derives): `name`, `inodes`, the six cache-size
  strings, `dismount_wait`, `upload_delay`, `fuse_io_uring_sqpoll_idle_ms`, `data_lv`,
  `meta_routing_width`, `meta_slot_runs`, `meta_volumes`, `encrypt_key` (§8).

**(c) Verbs and durability order (every verb; all offline verbs hold the D0 guard via the port's
layer-A flock = `status.writerGuardHeld`, `cli/format.zig:103-106`):**
1. live-writer gate on EVERY meta volume (a live daemon refuses even with `--force`);
2. read + decode the record (undecodable ⇒ refuse by name; `config_generation` read);
3. preflight (device identity probe §5.6(e); capacity closed form §5.5; path denylist);
4. stamp `VOLUME_LIFECYCLE` (bit 3) on every member superblock, barriered, iff the mutation
   introduces a non-`active` state or a second data member (the first time the set's data config
   diverges from its birth state — the bit's one meaning: "block keys may name a `vol_tag` this URI
   list does not serve; consult the record before resolving a tag"), BEFORE the record tx;
5. ONE conveyor tx on volume 0: `Put(set_config)` with `config_generation + 1` (+ the queued
   rebalance job record for `add-data`); for the ONLINE `add-data`: `publish_backend` strictly
   AFTER the ack ("placement enabled LAST", `fuse_client.rs:10370-10376`);
6. clean close (checkpoint).
`set-cache-paths` keeps the reference's wipe law (denylist, consent, 0700/0600, invoking owner
`cli/format.zig:68-73`). `remove-data` = preflight → `Active → Draining` tx → the ONE mover
primitive (charter §8) → `Retired` tx; `undrain` cancels the job records in the same tx.

**(d) The generation stamp.** `config_generation` is the config's own clock; the STAGING generation
stays `f(set_uuid, set_epoch)` from the membership stamp (charter §6.1 "generation-stamped to the
volume set's uuid-derived identity"), so a META set change (rung 5) bumps `set_epoch` and runs the
KD-8 drain/rebind (§5.3 verbatim: refuse on pending custody, two-phase dual marker); a DATA set
change never rebinds. The live daemon re-reads the record when `config_generation` moves (the
admin lane's own commit, or a poll of the record at the checkpoint cadence for a foreign verb).

**(e) THE DATA-VOLUME SUPERBLOCK — what `format` writes on a data volume.** The reference writes
NOTHING identifying (`main.rs:4523-4563`: zero `min(capacity, 32 MiB)` quick / the whole device with
`--full`, then `sync_all`); the mount opens `backing_dev` by path (`routing.rs:2951`) and the
duplicate check is `canonicalize(path)` (`config_ops.rs:806-838`). A renumbered `/dev/nvmeXnY`
across a reboot therefore binds a volume's `vol_tag` to the WRONG device silently — the exact class
charter §5.4 forbids for meta volumes ("order-independent mount discovery … URI-disagreement
refusals"). v4 gives every data volume an identity:

```zig
pub const DataSuperblockWire = extern struct {   // sector 0 AND the last aligned sector (self-located)
    magic: [8]u8,            // "SQZDATLV"
    version_le: [2]u8,       // 1
    reserved0_le: [2]u8,     // 0
    block_size_le: [4]u8,
    chunk_size_le: [4]u8,    // block + headroom(transform) | block
    compression: u8, encryption: u8, reserved1: [2]u8,
    volume_len_le: [8]u8,    // device length at format: the backup copy's self-location
    vol_tag_le: [8]u8,       // the record's id
    set_uuid: [16]u8,        // the META set's identity (the stamp's set_uuid)
    features_incompat_le: [8]u8,   // 0 today; unknown ⇒ refuse loud
    sb_generation_le: [8]u8, // DUR-5 newest-valid-wins between the two copies
    reserved: [40]u8,        // 0
    checksum_le: [8]u8,      // xxh3 over the 4096 B sector with the field zeroed, LAST
};
comptime std.debug.assert(@sizeOf(DataSuperblockWire) == 128);
```

`format` per data volume: zero chunk 0 and the last chunk (buries GPT/filesystem signatures at both
ends — the reference's 32 MiB wipe had no stated reason) → tail copy, barrier, sector 0, barrier
(sector 0 LAST so no crash prefix is a data volume). Chunks 0 and `last` are RESERVED from the
allocator (bits pre-set in the rung 6 data bitmap; `capacity_blocks = floor((volume_len −
2·chunk)/chunk)`). Mount discovery of DATA volumes: open each record's uri, read both copies,
refuse loud by name when `vol_tag ≠ record.vol_tag` ("`<uri>` carries vol-… but the set names
vol-…: the device was renumbered or belongs to another set"), `set_uuid ≠ stamp.set_uuid`, `block/
chunk/transform ≠ the record's`, or `Blank` ("not a formatted data volume"), or two uris read the
same `vol_tag`. `add-data` requires `Blank` (or `--force`) and writes the superblock BEFORE the
record tx (device-before-record, so a crash leaves an unlisted formatted device, never a listed
blank one). The `probe_data_volume_rw` write/readback probe (`config_ops.rs:850-894`) survives as
the superblock write itself read back.

**(f) `format` for the set (replaces `cli/format.zig:160-163`):** parse every `sqmeta://`/`sqdata://`
(≥ 1 of each, `main.rs:4197-4208`) → key material + salt + key_id BEFORE any destructive step
(`main.rs:4131-4171`) → validate every data device (`storage.rs:438-…`) → `capacity =
resolve(Σ physical, --capacity)` → block/chunk/headroom → preflight every meta volume → stamp staging
dirs → mint `set` identity + per-volume uuids + `vol_tag`s → write every DATA superblock → format
every META volume with the `set_config` record in volume 0's control tree (position 0's format tx)
→ summary (block, chunk, capacity, transform, key id + the three resolution sources, data volumes
with tags, cache paths or "cache-less"). Flags kept: `--block-size`, `--capacity`, `--compression
{none,lz4,zstd}`, `--encrypt-algo {none,aes256gcm,chacha20}`, `--encrypt-key <path|->`,
`--disk-cache-paths a,b`, `--force`, `--full`, `--meta-node-kib`, `--meta-journal-mb`,
`--single-writer`. Deleted: `--inodes`, the six cache-size flags, `--dismount-wait`,
`--upload-delay`, `--fuse-io-uring-sqpoll-idle-ms`, `--meta-slots` (already refused),
`--multi-writer`/`--symmetric` no-ops, `--data-lv`/`--meta-lv` aliases, `--ip/--port/--subnqn`.
`mount --disk-cache-paths` stays a LOUD instant error (`main.rs:2815-2831`).

---

## 6. Knobs, gauges/counters, constants

### 6.1 Knobs (registry rows; classes per `knobs.zig:57-72`)

| key | kind | class | note |
|---|---|---|---|
| `SQUEEZEFS_ENCRYPT_KEY_FILE` | Str (path) | `config` | resolution rung 2 (`keyfile.rs:31`; `env_knobs.rs:144`) |
| `SQUEEZEFS_DEFAULT_BLOCK_SIZE` | Int 4096..=BYTES_MAX, default 4194304 (`env_knobs.rs:165`; `routing.rs:7706-7711`) | **delete** | "when format did not record one" — v4 always records it; the DUR-8e fallback (`crypto_compress.rs:502`) has no caller |
| `SQUEEZEFS_FREE_FORENSICS` | Bool | — | reclaim's, not this spec's (`block_allocator.rs:96-104`) |
| `SQUEEZEFS_TEST_TRANSFORM_FORCE_RAW` | Bool | `test_seam` | **new**: forces the store-raw arm so the raw bit, the headroom and the window widening are exercised with compressible data |
| `SQUEEZEFS_TEST_CONFIG_GENERATION_SKEW` | Int | `test_seam` | **new**: pins the window between the record tx and the live mirror's adoption |

No A/B lever: compression/encryption are FORMAT facts, never mount-time postures (charter §1.6 "the
user never chooses a data path").

### 6.2 Gauges / counters

| name | kind | source / v4 |
|---|---|---|
| `compress_stored_raw` | ShardedCounter | `fuse_client.rs:6173-6179` |
| `nvme_unaligned_write_fallbacks` | ShardedCounter | `:6207-6215`; ≈ 0 on transformed volumes after PERF-10 |
| `key_unwrap_count` → `crypto_key_unwraps`, `crypto_key_unwrap_hits` | ShardedCounter | `crypto_compress.rs:183`, `:758-759` |
| `transform_frame_refusals{class}` | Tripwire per class (magic, version, flags, mode, len, aad) | v4 (the reference refuses with strings `:999-1017`) |
| `transform_plain_len_refusals` | Tripwire | DUR-8e (`:525-530`, `:555-559`) |
| `stored_image_overflow_refusals` | Tripwire (must-stay-0) | `block_allocator.rs:114-130` |
| `crypto_scratch_pool_{buf_len,capacity,heap_fallbacks}` | Gauge / counter | `:461-465`; heap fallbacks must be 0 in v4 |
| `transform_phase_ns{compress,seal,open,decompress}` | ShardedLatencyHistogram | v4 (the reference's `read_fill_phase_ns.decode`, `routing.rs:12164-12167`) |
| `volume_preflight_refusals` | ShardedCounter | `fuse_client.rs:6355` |
| `evacuate_{needed,avail,transient}_bytes`, `evacuate_blocks_moved`, `evacuate_shared_blocks_moved`, `evacuate_replans` | Gauge/counter | `:6323-6349` |
| `staging_drain_barriers` | ShardedCounter | `:7485` |
| `backend_placement_weight`, `backend_fill_ratio`, `backend_placement_picks` | Gauge per volume | `routing.rs:1404-1419` |
| `config_generation`, `data_volumes_{active,draining,retired,disabled}` | Gauge | v4 |
| `data_volume_identity_refusals` | Tripwire | v4 §5.6(e) |

### 6.3 Constants with WHY and measured numbers

| constant | value | WHY |
|---|---|---|
| frame word raw bit | bit 31 | image lengths ≤ 4 MiB class, so bit 31 was always 0 — strict-superset decoding (`crypto_compress.rs:40-43`) |
| `NONCE_LEN` | 12 | both AEADs' nonce (`:53`) |
| `AEAD_TAG_LEN_MAX` | 16 | both AEADs (`:75-78`) |
| `DATA_KEY_LEN`, `KEK_LEN` | 32 | 256-bit keys (`:71`, `keyfile.rs:49`) |
| `WRAP_V2_LEN` | 63 = 3 + 12 + 32 + 16 | (`:73`) |
| `KDF_SALT_LEN` | 32 | RFC 5869: the hash length (`keyfile.rs:45-46`) |
| `KEY_ID_LEN` | 8 | "enough to name a key file, far too short to be a key" (`:51-53`) |
| `MIN_KEY_MATERIAL_BYTES` | 32 | HKDF has no work factor: material, not a passphrase (`:36-39`) |
| `MAX_KEY_FILE_BYTES` | 64 KiB | "`--encrypt-key /dev/zero`-class mistakes" (`:41-43`) |
| `WRAPPED_KEY_LEN_FALLBACK` | 512 | retired RSA-4096 ghost in the geometry bound (`:55-63`) — **gone in v4** |
| `TRANSFORM_BLOCK_HEADROOM` | 4096 | one LBA ≥ worst-case non-payload bytes (547 ref / 96 v4) (`:80-103`) |
| `CHUNK_SIZE` | 4 MiB | allocator stride; a longer image tramples the next chunk (`block_allocator.rs:106-112`) |
| zstd level | 3 | library default (`:483`, `:797`) |
| scratch pool capacity | `(cores/4).clamp(4,16)` | FIFO rotation cache-cold +4..10 % at cores × 4 (`:21-32`) |
| scratch at 4 MiB | 4,214,784 B (= block + 20 KiB; envelope "+128 KiB") | §2.5 arithmetic; test `:1308` |
| unwrap cache | 10,000 entries | unstated (`:326`) — v4 derives |
| `process_write_async` threshold | 64 KiB | tokio `spawn_blocking` artifact (`:1045`) — v4 inline |
| `EVACUATE_INFLIGHT_WINDOW_BLOCKS` | 64 | per-worker in-flight double-count window (`jobs.rs:111-115`) |
| `DRAIN_HEADROOM_FLOOR_BYTES` | 1 GiB | foreground growth slack at zero rate estimate (`:117-119`) |
| `DRAIN_FLOOR_RATE_BYTES_PER_SEC` | 512 MiB/s | ½ of a ~1 GiB/s devsub-class copy for the INITIAL eta (`:121-125`) |
| `REBALANCE_DEFAULT_THROTTLE_PCT` | 25 % | conservative default of the auto rebalance (`:127-129`) |
| `REBALANCE_BAND` | 10 pp of set-mean fill | charter §8 "within 10 pp" (`:131-133`) |
| `BALANCE_PENALTY_K`, `_CAP` | 1000, 300 | `health_effective = health − clamp(1000 × (fill − mean), 0, 300)` (`routing.rs:1376-1399`; charter §6.1) |
| placement band | 90 % of max `health_effective` | (`:1421-1434`) |
| quick data wipe | 32 MiB | unstated (`main.rs:4525`) — v4: chunk 0 + the tail chunk, reasoned |
| `inodes` default | 1,000,000 | a display-only "quota" (`fuse_client.rs:29819-29844`) — deleted |

---

## 7. Tests (reference name → one sentence; v4 ports each as named)

**`crypto_compress.rs` unit (`:1072-1738`):** `test_crypto_unwrap_caching` — second decrypt hits the
cache, `key_unwrap_count == 1`; `test_session_key_reused_across_encrypts` — same 63 B blob, different
nonces/ciphertexts; `test_passthrough_process_write_is_identity` — same pointer out; `test_mode_parse`;
`test_lz4_roundtrip_process` — 64 KiB of 7s shrinks and round-trips;
`test_scratch_pool_sizing_uses_actual_prewrapped_key_blob` — blob is 63 B, buffer = formula, rounded to
the NEXT 4 KiB, `≤ bs + 128 KiB`; `…falls_back_to_the_conservative_wrap_bound` (512 — retired in v4);
`…init_idempotent_and_passthrough_noop`; `test_lz4_scratch_framing_byte_identical_to_prepend_size` —
deep then short writes through every pooled buffer leak no stale bytes, empty payload = lone RAW word;
`test_pre_scratch_volume_read_back_parity` (6 cases) — unframed legacy blob refuses, framed decodes,
device-window padding ignored; `test_scratch_pool_checkout_recycle_and_alignment` — one buffer, 4096
aligned, recycles; `test_incompressible_input_stays_pooled_and_stores_raw` — stored = ceil_4k(frame +
payload) = `max_stored_image_len`; `test_scratch_overflow_bounces_to_heap` (v4: no heap arm — refuses);
`test_uninitialized_state_stays_on_pre_scratch_heap_path` (v4: gone); `test_process_write_never_mutates_input`;
`test_scratch_pool_shared_across_clones_async`; `test_encrypted_scratch_header_parity` — header bytes
and length arithmetic `3 + 63 + 12 + ct + 16`.

**`encrypt_key_handling_tests.rs`:** `key_file_material_is_read_from_the_path`; `…trailing_whitespace_is_trimmed`;
`…refuses_group_or_other_permissions`; `…refuses_a_symlink`; `…refuses_a_non_regular_file`;
`…refuses_short_material`; `key_material_reads_from_stdin_shaped_input`;
`key_material_never_appears_in_debug_output`; `derivation_is_deterministic_and_salt_bound`;
`key_ref_carries_only_a_salt_and_an_id` (three fields, hex 64/16); `two_formats_of_the_same_key_file_get_different_salts`;
`wrap_unwrap_round_trips_under_both_record_algorithms`; `wrap_blob_is_versioned_magic_and_compact`
("SK", 2, 63 B); `unwrap_refuses_a_foreign_key_a_tampered_blob_and_a_bad_version` (names "version";
truncated never panics); `an_encrypted_block_round_trips_and_a_foreign_key_cannot_read_it`;
`an_encrypted_state_without_a_key_refuses_writes_loud` (names "key"); `format_config_never_serializes_key_material`;
`a_legacy_config_still_deserializes…`/`a_pre_kw1_encrypted_volume_refuses_with_an_exact_remedy`/
`an_encrypted_volume_with_no_key_reference_refuses` (v4: no legacy class — deleted);
`a_plaintext_volume_is_never_refused_by_the_key_posture`; `mount_resolves_the_key_from_the_env_path_and_verifies_the_id`;
`the_stashed_flag_material_wins_over_the_env_path`; `an_encrypted_volume_with_no_key_source_refuses_naming_every_source`;
`handling_key_material_makes_the_process_undumpable_with_no_core` (re-exec child);
`format_with_a_key_path_persists_only_a_key_reference`; `format_refuses_an_encrypted_volume_with_no_key_and_a_key_with_no_algo`;
`format_with_a_key_path_produces_a_mountable_writable_encrypted_volume` (the VAL-3 acceptance).

**`crypto_block_framing_tests.rs`:** `lz4_cold_striped_reads_decode`, `zstd_…`, `lz4_aes_…` — cold
device reads of framed blocks decode; `lz4_staged_growth_promotion_decodes`;
`frame_contract_padded_legacy_truncated_passthrough` — padded decodes, unframed refuses, truncated
refuses, passthrough byte-identical.

**`crypto_incompressible_tests.rs`:** `incompressible_{lz4,zstd,lz4_aes,zstd_chacha,aes_only}_striped_cold_roundtrip`;
`incompressible_extent_fold_cold_roundtrip`; `compressible_control_keeps_compressing` (`compress_stored_raw`
stays 0, image < raw); `incompressible_blocks_store_raw_flagged_frames`; `oversized_stored_image_refused_never_silent`
(a success claim must read back cold, `:566-607`); `mount_gate_refuses_pre_fix_transformed_geometry`;
`mount_gate_passes_clamped_and_passthrough_geometry`; `frame_marker_superset_of_pre_fix_encoding`;
`failed_incompressible_upload_keeps_old_data`.

**`crypto_scratch_pool_tests.rs`:** `test_set_crypto_inits_scratch_pool_from_block_size`;
`test_pooled_transform_dma_read_back_parity`; `test_pooled_transform_is_dma_aligned_no_bounce`.

**`volume_lifecycle_tests.rs`:** `test_format_config_data_volumes_round_trip_and_old_configs_decode`
(v4: encode∘decode = id + fuzz totality of the control record instead);
`test_legacy_data_lv_resolves_basename_ids_byte_identically` (v4: gone);
`test_new_volume_ids_are_vol_hex16_and_unique`; `test_lifecycle_bit_is_bit3_and_does_not_intersect_bits_0_1_2`;
`test_old_binary_mask_refuses_bit3_and_set_is_durable_idempotent`;
`test_offline_add_refuses_under_live_client_and_bad_devices`; `test_add_write_remount_read_and_new_backend_receives_allocations`;
`test_live_health_override_rehomed_and_phantom_alias_protected`; `test_offline_disable_is_durable_volume_state`.

**`volume_drain_tests.rs`:** `test_preflight_property_refuse_iff_closed_form` (2000 fuzzed censuses; the
refusal prints every number); `test_draining_volume_serves_reads_and_takes_no_placements`;
`test_drain_evacuates_retires_and_preserves_bytes_w1_ledger_zero`; `test_clone_shared_blocks_move_once_both_clones_intact`;
`test_stale_mapping_supersession_is_contractual_noop`; `test_quiescent_first_defers_live_buffers_then_converges`;
`test_remove_data_refuses_without_survivor_capacity`; `test_undrain_restores_active_and_cancels_the_job`;
`test_auto_rebalance_submitted_on_add_and_suppressed_by_opt_out`; `test_rebalance_moves_toward_the_set_mean`;
`test_offline_add_writes_durable_queued_rebalance_record`; `test_crash_mid_drain_resumes_by_replan_and_preserves_bytes`;
`test_offline_remove_data_drains_to_retired`; `test_close_returns_queued_reclaims_before_custody_release`.

**`cache_path_policy_tests.rs`:** `test_mount_rejects_disk_cache_paths_flag_loud_and_instant`;
`test_mount_adopts_format_declared_cache_paths`; `test_cacheless_mount_small_large_io_cycle`;
`test_set_cache_paths_admin_op_end_to_end`; `test_cache_path_ops_edge_cases`;
`test_cacheless_tiered_cache_library_contract`.

**v4 additions (red-first):** `TransformFrameWire` encode∘decode = id and decoder totality under
`std.testing.fuzz` (every refusal class a named tripwire); AAD covers the header (flip any header
byte ⇒ open fails); `plain_len > block` refused before allocation (counting allocator pins zero);
`chunk − max_stored_image_len(block) ≥ 0` tie test restating `96 + block ≤ block + 4096`;
`DataSuperblockWire` encode∘decode, backup self-location, newest-valid-wins, `Blank` classification;
data discovery refusals by name (renumbered uri, foreign set, duplicate tag, geometry disagreement);
`VolumeSetConfigWire` totality + the record-cap refusal at the verb; verb durability order under the
kill matrix (bit 3 ∧ ¬record ⇒ mounts unchanged; record ⇒ whole; superblock-before-record for
`add-data`); `config_generation` strictly monotone across every verb; the capacity closed form as a
property test; key-file laws per class with `log.testing.expectErrorLogs`.

---

## 8. QUIRKS (0002 ledger-row candidates) and the better design

| # | reference quirk | why poor | v4 |
|---|---|---|---|
| 1 | Volume-set config as a `serde_json` xattr `user.squeezefs.format_config` on ino 1 (`builder.rs:59`, `config_ops.rs:21-40`), optional additive fields, no version/checksum, rewritten whole, REQUIRED by fsck (row 8), reached by a `slot0_root_ino` indirection (`:24-27`) | A serialization-library artifact as the set's durable truth (row 2's class); a user-namespace xattr as a mount bootstrap (row 46) | ONE versioned, checksummed control record `set_config` on volume 0's tree 0, canonical total codec (§5.6 b) |
| 2 | `data_lv` path list MIRRORED beside `data_volumes` on every write (`config_ops.rs:1440-1446`), and `None` data_volumes on a fresh format "legacy-shaped" (`main.rs:4356-4360`) | Two spellings of one list; a fresh set born in a compatibility shape | One list from birth |
| 3 | Grandfathered ids = device basenames hashed with xxh3 (`lib.rs:1342-1349`, `block_refs.rs:200-201`) | A path fragment as a permanent key component; two tag derivations for one field | `vol_tag` u64 IS the id, random, from birth |
| 4 | A DATA volume carries no identity: `format` zeroes 32 MiB (`main.rs:4523-4563`), mount opens by path (`routing.rs:2951`), duplicates by `canonicalize` (`config_ops.rs:806-838`) | A renumbered device binds a tag to the wrong disk silently — the class §5.4 forbids for meta | `DataSuperblockWire` at sector 0 + self-located tail copy; discovery refuses URI disagreement by name (§5.6 e) |
| 5 | Mixed endianness and always-constant length bytes in the AEAD header (`crypto_compress.rs:711-713` BE u16 + u8 beside the LE frame word); lz4's prepend-size nested inside the image (`:479`, `:788-795`); two zstd frame shapes (`:483` vs `:797`) | Not one canonical codec; library framing as on-disk format | ONE 96 B `TransformFrameWire`, LE, `plain_len` in the header, one zstd shape (§1.3) |
| 6 | Empty AAD on block AEAD (`:706`, `:882`) | The raw bit and lengths are unauthenticated | AAD = the header |
| 7 | `WRAPPED_KEY_LEN_FALLBACK = 512` (retired RSA-4096) sizing the on-disk headroom and every read window (`:55-63`, `:395-400`, `routing.rs:12145-12153`) | A dead primitive's size as a format constant | header is fixed 96 B; headroom = `ceil_4k(96)` |
| 8 | `format` shrinks the block to `CHUNK − 4096 = 4,190,208` on transformed volumes (`main.rs:4326-4339`) | A non-power-of-two logical block; every boundary leaves the 1 MiB writeback grid | `chunk = block + headroom`; the block stays what the operator named (§4) |
| 9 | `--inodes` "hard quota" default 1,000,000 that only feeds `statfs.f_files` (`main.rs:46-48`, `fuse_client.rs:29819,29844`) | A free-floating constant (§1.5) presented as a quota | Deleted; `f_files` derived from metadata capacity |
| 10 | Cache/tuning strings in the FORMAT config: `mem_cache_size "1GB"`, `disk_cache_size "10GB"`, 50 % read/write splits, `dismount_wait`, `upload_delay "500ms"`, `fuse_io_uring_sqpoll_idle_ms`, `name` (`lib.rs:1375-1404`, `main.rs:49-66`) | Free-floating byte/time constants frozen on disk; the R5 budget authority derives these | Deleted from the record |
| 11 | `meta_routing_width`, `meta_slot_runs`, `meta_volumes` "MIRROR ONLY" copies of the stamps in the config (`lib.rs:1405-1421`) | A second home for the membership truth (rows 41, 46) | Not recorded |
| 12 | `new_kdf_salt` falls back to `fastrand` + clock when `SystemRandom` fails (`keyfile.rs:209-222`) | A weaker posture taken silently (§1.8) | `Io.randomSecure` failure refuses the format |
| 13 | Key file owner law: root accepts ANY owner, non-root requires `st_uid == euid` and refuses root-owned (`keyfile.rs:380-388`) | Differs from the charter's "owned by euid/root"; root loading a user-planted file | `st_uid ∈ {euid, 0}` for every euid |
| 14 | `--encrypt-key` material in a process-global `Mutex<Option<KeyMaterial>>` crossing `fork` (`keyfile.rs:409-424`) | A global as the parent→child channel | The CLI's daemonize path hands the material explicitly (`cli/daemon.zig`), zeroized in the parent after the fork |
| 15 | Mode strings parsed per state with `-rsa` aliases "retained so a pre-KW-1 config still parses far enough to be refused" (`crypto_compress.rs:114-116`, `:141-144`); `legacy_encrypted_volume_refusal` and the cleartext `encrypt_key` field kept to name a defect (`keyfile.rs:482-522`, `lib.rs:1359-1367`) | A legacy class the port never had | u8 enums; no legacy arm |
| 16 | moka unwrap cache of 10,000 (`:326`); `process_write_async` 64 KiB `spawn_blocking` threshold (`:1045`); heap `Vec` fallback when the pool is uninitialized or overflowed (`:946-963`) | Unstated constants; a tokio artifact; a per-op allocation arm on the data path (§12) | Derived cache; inline on the lane; pooled only (refuse past the pool's basis — a shape the product never produces) |
| 17 | `SQUEEZEFS_DEFAULT_BLOCK_SIZE` as the DUR-8e fallback bound "when format did not record one" (`env_knobs.rs:165`, `crypto_compress.rs:502-505`) | A knob standing in for a format fact | Deleted; the record always carries `block_size` |
| 18 | Quick data wipe of `min(capacity, 32 MiB)` (`main.rs:4523-4530`) with no stated reason | An unreasoned constant | Zero chunk 0 and the tail chunk (both reserved; both signature regions) |
| 19 | `KEY_WRAP_SCHEME_V2` persisted as a 22-char string, salt/key_id as hex strings (`keyfile.rs:172-180`) | Text where the codec is binary | `key_scheme u8 = 2`, `kdf_salt [32]u8`, `key_id [8]u8` |
| 20 | `VOLUME_LIFECYCLE` bit 3 stamped because the JSON's additive `data_volumes` was invisible to old binaries (`config_ops.rs:1425-1438`) | The bit's reason is a codec defect | Kept with ONE meaning (§5.6 c step 4): the data set has diverged from its birth state — a `vol_tag` may name a volume the URI list does not serve |

---

## 9. Delivery order inside rung 4 (each red-first)

1. `data/transform.zig` (portable): `TransformFrameWire`, the per-block best-effort rule, the raw
   bit, AAD, nonce, DUR-8e bound, `maxStoredImageLen`, `headroom`, `geometryCheck`; vendored lz4/zstd
   behind one `Codec` seam; `std.crypto` AEADs; fuzz totality.
2. `data/keyfile.zig` (Linux: `prctl`, `setrlimit`, `O_NOFOLLOW` through `std.os.linux`; the pure
   derivation portable): HKDF labels, the 63 B wrap, resolution order, file laws, hardening, zeroize.
3. `data/data_superblock.zig` (portable codec) + `meta/set_config.zig` (the control record codec +
   verbs' pure state machine: `apply(verb) → {bit_needed, record', generation+1}`).
4. `cli/format.zig`: the set format (§5.6 f); `cli/volume.zig` / `cli/config.zig`: the verbs with the
   durability order of §5.6 (c); discovery refusals; the capacity closed form with its property test.
5. Evidence note `docs/evidence/rung-04-data-transform-and-config.md`: the measured scratch size at
   4 MiB (4,214,784 B), `compress_stored_raw` on the incompressible matrix, zero heap fallbacks
   (counting allocator), the kill matrix over every verb.
