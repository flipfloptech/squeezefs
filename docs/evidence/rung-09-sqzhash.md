# Rung 9 — sqzhash128: the ONE hash of the format and the wire

The owner's decision (2026-10-11, decision 0003 §M; 0002 row 480): adopt **sqzhash128** — the
owner's keyed AES-round 128-bit hash from `~/Source/zig_hash_bench` — as the one hash of the
on-disk format and the wire, replacing xxh3-64 everywhere the tree used it. This note is the
evidence: the pinned definition and its vectors, the path-equality proof, the facade, the per-mount
name key, the format act and its refusal, the measurement on both venues, and the gate totals.

Venues: the dev box (Apple M3 Pro, macOS 27.0.1, Zig 0.17.0 — the portable gate) and the Linux
venue (`7.2.6-sqz`, Ubuntu 24.04, 9 vCPU Apple-VM aarch64 with the AES extension, 16 GiB; two
other agents' gates sharing it, load 1–3 during every row below). Both builds ReleaseSafe (the
`gate` profile — what the product ships).

## 1. The definition is pinned (`src/core/sqzhash.zig`)

Ported bit-for-bit from the source (`zig_hash_bench/src/sqzhash.zig`, 793 lines) on
2026-10-11; the file's header names its provenance and the law: THIS FILE IS THE FORMAT'S
DEFINITION — any output change is a format change. The construction is the source's (one AES round
`F` without a key, `absorb(a, b) = F(a ^ b)`; a `Key` of 20 vectors expanded from a 128-bit secret
with two rounds each; one block ≤ 16 B; a tree of 2/4/6/8/12/16 leaves to 256 B where no state
absorbs two blocks; 8 lanes to 1 KiB and 16 beyond; `mergeLanes` with its three-round argument;
a two-round finalizer), re-shaped only as `Engine(comptime path)` so the tests can instantiate the
hash on BOTH round-function paths at once.

**Bit-exactness with the source** — the source's own 24 fixed vectors (key 0, lengths 1…4242),
`hash(0, "") = 0xb834…0461`, `hash(0, "Hello World") = 0x3109…fe6e`, `hash(42, buf[0..4242]) =
0x4e80…a8cc` — all pass unchanged (`test "source vectors: the port is bit-exact …"`).

**The format's pinned vectors** — 28, produced ONCE by the source implementation and asserted
forever (`pinned_format_vectors`, `pinned_seed_vectors`): `fillTestData` inputs (`i·31 + 7`) at
lengths 0, 1, 7, 16, 17, 48, 100, 128, 256, 257, 1 KiB, 4 KiB, 64 KiB + 1, 256 KiB, under the
FORMAT KEY (`0x5351_5A46_5320_464F_524D_4154_204B_4559`, the ASCII of `SQZFS FORMAT KEY`) and
under a volume seed widened by the name-key rule (`0xF1D5_A000_0000_0004` zero-extended — the
FIND-VS-A fixture's seed), each as the u128 AND its low 64 bits, one-shot AND streaming. Three of
them, for the record:

```
format key, len 0        f5d2588c13349a3b60faebc5c5bda514   low64 60faebc5c5bda514
format key, len 4096     205bc4c59b3a297443f67887f27d66b8   low64 43f67887f27d66b8
format key, len 262144   ce66013faa999bc9a61e7f074079227e   low64 a61e7f074079227e
```

## 2. Path equality: the three round functions are one function

The round primitive has three implementations in the file: the AArch64 `AESE`/`AESMC` pair (what
the product runs on both venues), `std.crypto.core.aes`'s round (AES-NI on x86_64 with AVX, the
std software round elsewhere), and a table-driven software round (FIPS 197 §5.1.1–5.1.3: the
S-box built from the field inverse `a^254` and the affine map, ShiftRows, the `(2 3 1 1)`
circulant) that runs only under `@inComptime()` — the FORMAT key's expansion at compile time.
The proof is in the tests:

- `Engine(.native)` and `Engine(.portable)` produce identical output on every pinned vector and on
  every one of the 42 test lengths (0 … 20,000) under three keys; their expanded keys are equal
  vector for vector.
- The primitive on both paths equals the software round on 10,000 random `(lane, block)` pairs,
  the S-box pins `S[00]=63, S[01]=7c, S[53]=ed, S[ff]=16`, and FIPS 197 appendix B's round-1
  state (`19 3d e3 be … 48 08` → `04 66 81 e5 … 26 4c`) passes through all three.
- A comptime `Key.init(FORMAT_KEY)` equals the runtime expansion vector for vector
  (`checksum.format_key` is the comptime one).

Cross-platform equality is therefore by construction: x86_64 only ever runs `std.crypto`'s round,
which the tests tie to the same textbook round the AArch64 path is tied to.

Streaming ≡ one-shot: every test length × 12 split sizes (1 … 20,000) × 3 keys, `final` and
`finalLow64`; `final` does not disturb the state. The source's distribution tests (every input bit
affects the hash; repeated and swapped blocks; the SMHasher sparse keyset on the low 64; the
crafted one-round cancellation up to 256 B; avalanche 60–68 of 128 bits) all pass on the port.

## 3. The facade (`src/core/checksum.zig`) and the two keys

Algorithm-neutral names: `hash64(bytes)`, `hash128(bytes)`, `hash64Keyed(key, bytes)`,
`hash64FieldZeroed(bytes, off)`, `stamp`/`verify` (unchanged names), `low64(h128)`, `Key`, the
streaming `Hasher` (`init()` under the format key, `initKeyed(key)`, `update`, `final` u128,
`finalLow64`). Every 8-byte field is the LOW 64 bits — the half SMHasher3 verified on its own.

- `FORMAT_KEY` is fixed and public ON PURPOSE (a checksum detects corruption and keeps no secret;
  sector 0 must verify before any per-volume value is known); `format_key` is its comptime
  expansion — no mount pays the 40 rounds.
- `keyFromSeed(hash_seed)` zero-extends the volume's 64-bit seed to the 128-bit secret; the rule
  is the format's (the name hashes on disk depend on it); `FORMAT_KEY`'s non-zero upper half keeps
  every seed disjoint from it.
- `key.NameKey` (`src/meta/key.zig`) wraps the expanded name key as a distinct type, made ONCE at
  open (`KvBackend.name_key`, `MetaSet.name_key`, `ImageBuilder.name_key`, the fsck/test stores'),
  and `dentryNameHash54(name, *const NameKey)` / `xattrNameHash56` take it by pointer. 47 call sites
  read it through a store's `nameKey()`; no path expands a key per name.

### 3.1 The streaming state is the construction's minimum (resolved 2026-10-12)

The adoption's one recorded caveat (0003 §M.5): the staged cell's value-first streaming state
(`staging.ValueHash = checksum.Hasher`) grew from ~600 B (xxh3's) to **4,736 B** per LIVE cell. Measured
(`@sizeOf`, both venues): a COPY of the expanded `Key` (320 B: 20 vectors), the 16 lane states (256 B),
a **4,096 B** pending buffer (16 chunks — the source's streaming shape, sized for throughput, not for a
resident state), two counters and 48 B of padding to the key's 64-B alignment. One such state lives per
live staged record; the ring's cell population is bounded by `capacity / 4 KiB + 1` (`Segment.maxCells`),
so at the venue's measured 546 MiB ring (`rung-09-memory-footprint.md`) 139,777 cells could hold
**662 MB** of hasher state — 116 % of the ring they describe, 14.5 % of any budget at the `budget/8`
ring ceiling — and none of it was a budget component (R5's Σ did not see it).

The derived minimum (`sqzhash.zig` `Hasher`, the tie test "the streaming state is the construction's
minimum"): the key by POINTER (the format key is a comptime constant, the name key lives once per
mount), and ONE of two phases, a union — `prefix`, the input itself while it is ≤ `medium_max` (1 KiB:
every regime below 16 lanes — one block, the 2…16-leaf tree, 8 lanes — is a function of the WHOLE
input in a shape the 16-lane state cannot produce, so `final` hashes it one-shot), else `lanes`, the
16 lane states (256 B) and the last 256 B seen (the final chunk is aligned on the END of the input and
may start at any byte; a chunk is absorbed only once a byte FOLLOWS it). Size: `1024 + 16 (tag, at the
vector alignment) + 8 (length) + 8 (key pointer) = `**1,056 B**, pinned as an equality. The OUTPUT is
unchanged — the 28 pinned vectors, the source's 24, streaming ≡ one-shot over 42 lengths × 15 splits
(now including fills that land short of, on and past the 1 KiB switch) × 3 keys, and the facade's
split test all pass unchanged on both venues.

| | per live cell | 64 MiB ring (16,385 cells) | 546 MiB ring (139,777 cells) | share of the ring | share of the budget at `budget/8` |
|---|---|---|---|---|---|
| before | 4,736 B | 77.6 MB | 662.0 MB | 116 % | ≤ 14.5 % |
| after | **1,056 B** | 17.3 MB | 147.6 MB | 25.8 % | ≤ 3.2 % |

The states are now GAUGED: `staging_cell_state_bytes` (every `create`/`destroy` of a cell's state in
`Segment`, through one pair of helpers) is the budget component **`staging_cell_state`** (attribution-
only, weight 0 — a state leaves with its cell, and the never-lossy lever that empties cells is
`staging_mmap`'s promotion kick; heap, not page cache), registered beside `staging_mmap` at
`openStaging`; `Segment.cellStateBound(capacity) = maxCells × @sizeOf(ValueHash)` is the bound the test
restates, and a comptime rail pins `@sizeOf(ValueHash) < RECORD_ALIGN` (the RAM beside a record never
outweighs the grain the record occupies). Test: "the live cells' value-hash states are gauged (R5)" —
N puts carry N × 1,056; an in-place append swaps a state for a state; a same-key replace frees the old
one with the last lease; a release frees one; `deinit` the rest; a recovered record carries none until
its first append. Every other `checksum.Hasher` (21 sites, on fiber stacks) shrank with it.

Facade pins: `hash54("hello", Key(42)) = 0x57efcfaac4954437 >> 10`,
`hash56("user.layout", Key(42)) = 0xe67f1d17ff5c7b1c >> 8`; `hash64("") = 0x60faebc5c5bda514`;
`stamp` → `verify` round trips and every single-byte flip over a 4 KiB page fails `verify`;
`hash64FieldZeroed` equals hashing a copy with the field zeroed.

## 4. The sweep (one commit, `64a5e20`; 89 source files)

Every `checksum.xxh3*` call site renamed (73 `hash64`, 7 `hash64FieldZeroed`, 11 `hash64Keyed`
via the name key; 38 `stamp`, 11 `verify` unchanged); every direct `std.hash.XxHash3` streaming use
(the wire frame, the enroll record, the journal entry and page, the ledger slot, the node header,
the control-record spill, the writer claim and PR key, the appender directory, the nvmet ledger,
the staging identity and value-first record state, the join token, the builder digest, the grant
ledger's writer key, the fabrics sampler, the migration tee, the SDK's test) through
`checksum.Hasher` (21 sites); the field renames `xxh3_le → csum_le`, `XXH3_OFF → CSUM_OFF`,
`BlockChecksum.xxh3 → csum`, `read_xxh3 → read_csum`, `entries_xxh3_le → entries_csum_le`; every
`@offsetOf` and size assertion unchanged and still true. `std.hash.Wyhash` for the model checker's
state fingerprints (RAM-only, like the fuzz corpus's names). `std.hash.XxHash3` survives in exactly
two lines: the bench's reference column and its test.

## 5. The format act and its refusal

Metadata **v5** (`superblock.version_v5`): `classifySector0` names a `SQZMETLV` sector with
version `< 5` LEGACY before any checksum check, so a v4 image (xxh3-64 everywhere) is refused with
the lever and never as a tear — the test: a v4 header over a valid v5 image, and the same with a
flipped byte, both classify `legacy = 4`; the mount refusal reads *"Metadata volume /dev/meta0 is a
pre-v5 squeezefs format (version 4), which this binary does not mount — reformat required: run
`squeezefs format` (destroys the old contents)"*; version 99 stays "unsupported … upgrade". The data
superblock moves to v2 (a v1 image and a v3 image both refuse by version first), the framed block
image to `FRAME_VERSION 3`, the staging segment record to v3 and the marker to v2, the NVMe
read-cache frame to v2. `tests/fixtures/findvsa_pred_bset.v5.bin` regenerated from the in-tree
generator (254,932 bytes, 3,515 records — the same size, every checksum and `hash54` different);
the pin test compares it byte for byte on every run.

## 6. The measurement (`squeezefs bench --hash`, both venues)

A new bench mode, CPU only and portable, no mountpoint. Four shapes (16 B name, 100 B record,
4 KiB page, 256 KiB node) × three columns (`xxh3-64` = `std.hash.XxHash3`, the hash the tree used;
`sqzhash128` = the full result under the format key; `sqzhash64` = its low 64, what every format
field pays) × two modes: THROUGHPUT (independent hashes the core overlaps — a stream of page
checksums; the input pointer is memory-clobbered each iteration so nothing hoists) and CHAINED
(the previous result written into the input before the next hash — a probe chain's dependent
latency). Each cell doubles its batch until the span covers 100 ms (a 20 ms span read a 100 B cell
2.3× apart between runs on the shared venue).

### The Linux venue (`b135c250c256`, the committed baseline's run)

| shape | xxh3-64 tput | sqzhash128 tput | sqzhash64 tput | xxh3-64 chained | sqzhash128 chained | sqzhash64 chained |
|---|---|---|---|---|---|---|
| 16 B | 2.171 ns · 7.4 GB/s | 1.347 ns · 11.9 | **1.350 ns · 11.8** | **5.150 ns** | 6.130 ns | 5.899 ns |
| 100 B | 8.549 ns · 11.7 | 6.875 ns · 14.5 | **3.110 ns · 32.1** | 8.686 ns | 6.821 ns | **6.457 ns** |
| 4 KiB | 3670.6 ns · 1.12 | 29.48 ns · 139.0 | **29.41 ns · 139.2** | 3668.5 ns | 29.33 ns | **29.51 ns** |
| 256 KiB | 207.0 µs · 1.27 | 2.604 µs · 100.7 | **2.595 µs · 101.0** | 207.9 µs | 2.593 µs | **2.618 µs** |

### The dev box (Apple M3 Pro, `60cfc3491099`)

| shape | xxh3-64 tput | sqzhash128 tput | sqzhash64 tput | xxh3-64 chained | sqzhash128 chained | sqzhash64 chained |
|---|---|---|---|---|---|---|
| 16 B | 2.449 ns · 6.5 GB/s | 1.355 ns · 11.8 | **1.355 ns · 11.8** | **5.293 ns** | 6.236 ns | 6.851 ns |
| 100 B | 10.609 ns · 9.4 | 3.524 ns · 28.4 | **3.529 ns · 28.3** | 10.673 ns | 6.990 ns | **6.825 ns** |
| 4 KiB | 3744.3 ns · 1.09 | 30.06 ns · 136.3 | **30.17 ns · 135.8** | 3753.8 ns | 30.08 ns | **30.12 ns** |
| 256 KiB | 218.0 µs · 1.20 | 2.524 µs · 103.9 | **2.535 µs · 103.4** | 220.0 µs | 2.583 µs | **2.541 µs** |

Readings. (1) The product's hash (`sqzhash64`) is 1.6× faster than xxh3-64 at 16 B throughput,
2.7–3× at 100 B, and **125× / 80× at 4 KiB / 256 KiB**: `std.hash.XxHash3`'s stripe is a
`@Vector(8, u64)` `lo32 × hi32` multiply that AArch64 NEON has no instruction for — LLVM
scalarizes it, and the accumulate loop runs at ~1.1 GB/s on this core (the owner's hand-written
xxh3 in `zig_hash_bench` reaches 46–60 GiB/s with `UMLAL` splits; std's does not). Every node
header, bset and 4 KiB page checksum paid that rate: a 256 KiB node verify was a 208–220 µs stall
and is 2.6 µs. (2) The one shape xxh3 keeps is the 16 B DEPENDENT chain (5.15 vs 5.9 ns: one
multiply-fold against four dependent AES rounds) — the owner's own latency table records the same
exception (5.2 vs 5.9); at 100 B and up the chain is sqzhash's too. (3) `sqzhash128` and `sqzhash64`
are the same computation; on the venue's Linux build the 128 column reads 6.9 ns at 100 B
throughput against the 64 column's 3.1 (reproducible across three runs, equal on macOS) — the u128
return is what differs; the format pays the 64-bit projection, which is the judged column.

### The baseline (`bench/baseline-7.2.6-sqz__arm-impl0x61-part0x000-9c__file-ext4.json`)

`zig build bench-baseline` re-recorded on the venue (3 runs; the rig runs `bench --hash --json`
after each mounted run and folds the `sqzhash64` throughput rows): `hash_16b` 730.8 M/s ± 15.3 M
(drop max 46.0 M), `hash_100b` 319.0 M/s ± 2.4 M (drop max 15.9 M), `hash_4k` 33.98 M/s ± 31 k
(drop max 1.70 M), `hash_256k` 384.2 k/s ± 835 (drop max 19.2 k); p99 is 0 for a hash group (no
histogram; the judge's rise bound is 0 and never fires). The six mounted groups re-recorded in the
same act (write_seq_1m 1495 ops/s, read_seq_1m 9343, read_rand_4k 19893, write_rand_4k 8185,
stat 5353, del 4891).

## 7. Gates

- **macOS** (`zig build gate`, portable modules): fmt, unit, rails (16/16: the literal allowlist
  rebased across 68 line shifts, no new row), smoke/bench-smoke/shim/nvmet skip off the venue.
  Unit totals: see §8.
- **Linux venue** (`timeout 3900 zig build gate --summary all`): fmt, unit, rails + shim census,
  smoke (the kill matrix: 61 pass, 2 skip), bench-smoke 40, shim-smoke 40, nvmet-smoke 51,
  suites-smoke, sdk-smoke. fsck's C-passes verify every checksum under the new definition inside
  the smoke's rows.

## 8. Totals

The final gate runs on the rebased branch (on `1facd8a`), both venues, both green (the one red in
the first pass of each was the literal rail over the bench's fill line, a comment; re-run green):

| venue | unit | rails | smoke | bench-smoke | shim-smoke | nvmet-smoke | wall |
|---|---|---|---|---|---|---|---|
| macOS M3 Pro | 1956 pass, 25 skip (1981) | 16/16 | skip (36 rigs) | skip (36) | — | skip (36) | 6 min 43 s |
| Linux venue | 2394 pass, 42 skip (2436) | 16/16 + census 1 | 64 pass, 2 skip | 40 | 40 | 51 | 12 min 9 s |

One pre-existing flake met once on the venue during this work and not again:
`stats.stats.test.allocation law … exactly ONE allocation` read 18 allocations against 10 in a full
run under the other agents' load (passes alone and in every other run; the count is a process-global
registry's, not the hash's).

## 9. What changed, in one list

- `src/core/sqzhash.zig` (new, the pinned definition), `src/core/checksum.zig` (the facade).
- §3.1 (the day after): `sqzhash.Hasher` holds the key by pointer and a two-phase union (1,056 B,
  pinned, from 4,736; output unchanged); `staging.stats.staging_cell_state_bytes`, the component
  `staging_cell_state`, `Segment.cellStateBound`, the `@sizeOf(ValueHash) < RECORD_ALIGN` rail.
- `src/meta/key.zig` `NameKey` + `hash54`/`hash56` by pointer; `nameKey()` on `KvBackend`,
  `MetaSet`, `MemStore`, `FaultStore`, the census `Store` vtable, `HolderStore`; `name_key` on
  `ImageBuilder`, `DataPlane`, `pack.CommitCtx`, `layout.Publish`, the fsck `Host`, economy's
  stage params, `tx.dentryInsertKey`, the stripe map's `routeName`/`locate`/`readdirMerged`/
  `verifyStripes`.
- The format versions: `superblock.version_v5` (+ the LEGACY classification of `< 5`),
  `data_superblock.version_v2`, `transform.FRAME_VERSION = 3`, `staging.SEGMENT_RECORD_VERSION = 3`,
  `staging.MARKER_VERSION = 2`, `nvme_read_cache.FRAME_VERSION = 2`; the fixture `.v5.bin`.
- `src/cli/bench.zig` `hash_bench` (`--hash`, `--json`), `args.zig` `--hash` (no mountpoint),
  `bench_baseline.zig` groups `hash_16b/100b/4k/256k` + `Group.ofHashRow`, the rig's second
  invocation; the venue baseline and thresholds re-recorded.
- Docs: charter §5.1 (checksums everywhere, `hash54`, the version), a checksum-amendment note atop
  22 porting specs with the law lines amended inline, the knobs doc for
  `SQUEEZEFS_NVMET_PORT_ID_BASE`, decision 0003 §M, 0002 row 480, this note.
