# 2026-07-30 — Write commit economy: per-ino publish coalescing + layout delta records

Branch `perf/write-commit-economy` (off dev `e076db7`, **unmerged — do
not merge/push without orchestrator review**; the orchestrator runs the
field protocol after these gates). The prong-2 program filed by
`.benchmarks/2026-07-30-meta-plane-writes.md` §3, designed and landed.

Commits: red contracts `1e6631e` (layout delta wire +
fold/economy/refusal pins), lever 2 `07d02f3` (fold integration +
`merge_layout_and_size` + `KV_LAYOUT_DELTAS` bit), lever 1 `bac3cd9`
(publish conveyor + delta-wired saves + kill-9 soak), stats/knob
registration `699d8da`, doc-link fixes `432c2d0`, the A-B-B-A bracket
rig `8882596` + meta-cell fix `6785f0b`, the rewrite/displacement
contract `7cbe9f9`, docs/evidence (this note).

## 1. Motivating data: the falsification + the conviction (field, e076db7)

Controlled experiment on the user's 4-node cluster, TODAY, binary
e076db7: the meta-volume distribution fix engaged perfectly — both
journals balanced (~18 k w/s each, sub-ms latencies, ≤ 36 % util) —
and **write throughput did not move** (fresh 10.3 / rewrite 6.7 GB/s
vs reads at 20). The meta-IOPS-ceiling hypothesis is **falsified**.

The standing mechanism: **per-file commit serialization with
O(file-size) per-publish cost**. Arithmetic: 1,670 rewrite blocks/s ÷
32 files ≈ 52 publishes/s/file ≈ **19 ms per publish cycle per file**
(fresh ~12.5 ms). Terms (meta-plane-writes §1/§3 audit):

- `merge_block_mappings` → `set_layout_and_size` re-serialized the
  WHOLE block map per 4 MiB publish (1.2 KB → 18.2 KiB mean journal
  entry as files grow — O(file_size) journal bytes, O(n²) per streamed
  file);
- `set_layout_and_size` = 123,722 of 145 k field commits;
- conveyor `meta_commit_group_size` median 1 on streaming shapes
  (serial per-ino commits — nothing batches);
- the write pipeline parked 39/40 admitted blocks awaiting these
  commits (depth governor exonerated).

## 2. The design

### 2.1 Lever 2 — layout delta records (`src/layout_wire.rs`, fold in `record.rs`)

The §4.2 fold algebra (ONE function: point lookup, bset merge,
compaction, fold-forward heads, journal replay) gains a second delta
class. A `Delta`-kind record on the layout xattr key whose payload
leads with magic `0x4C31` carries:

- the publish batch's `(block → key)` **map inserts** (O(batch)), and
- **every non-map layout field absolutely** (`file_type`, `size`,
  `block_map_id`, `block_prefix`, `file_id`, `data_key` — "full minus
  map"), so the only state the fold inherits from the base is the block
  map itself — which is mutated exclusively through persisting merge
  paths under `INODE_META_LOCKS`. Staged-identity flips, deferred size
  floors, and promotions ride the delta verbatim: the RAM authority and
  the fold-reconstructed value cannot diverge on any non-map field.

Fold: newest-first scan collects deltas until the base `Put`
(unchanged); the delta chain applies ascending onto the decoded
`LayoutMetadata` and re-encodes **canonically** (the block map now
serializes in ascending block order — decode-compatible with every
stored value; folds are byte-deterministic, so replay-twice digest
equality and `fold_forward ≡ fold_newest_first` hold at the byte
level). Refusals are loud, never guessed: indirect (`indirect:`) bases,
legacy-JSON bases, mixed delta classes on one key ⇒ `Corrupt`.

Eligibility is a two-half ladder:
- **backend half** (`KvMetaBackend::merge_layout_and_size`): a live,
  non-JSON base at the xattr slot (one point lookup under the held
  I-guard) + the incompat ratchet; otherwise the caller-provided full
  layout is staged (always-correct fallback).
- **caller half** (`CachedMetadata::layout_delta_chain`): provenance
  from fetch (bincode-inline base ⇒ eligible at chain 0; JSON/indirect
  ⇒ ineligible), re-based to 0 by every full inline save, deepened by
  each delta save, capped at `SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN`
  (default 64 — bounds cold-read fold chains and per-key replay work;
  `0` = lever-2 A/B kill). Synthesized cache entries default
  INELIGIBLE (conservative re-base).

**Format**: `KV_LAYOUT_DELTAS` = superblock `features_incompat` bit 5
(forward-only). KD-14 ordering: the bit is stamped + barriered
(`sync_device`) **before** the volume's first delta record can be
durable; pre-campaign binaries refuse the mount loud; volumes that
never stage a delta stay bit-identical.

### 2.2 Lever 1 — per-ino publish coalescing (`DataRouter::merge_block_mappings_coalesced`)

Block publishes enqueue on a per-ino conveyor (reusing the
loom-modeled `ConveyorCore` — the M7 commit conveyor pattern one level
up). A leader-elected **detached** pass (the M7 cancellation-safety
law: no client-visible cancellation can drop a batch mid-commit)
drains whatever accumulated during the previous commit — **no timers**,
the jbd2/M7 no-wait shape — applies the whole batch under ONE
`INODE_META_LOCKS` section with the direct primitive's exact per-op
semantics (dirty-authority RMW base, per-op fencing — a stale op fails
ALONE with `FencingTokenExpired` and publishes nothing, displaced-key
purge, size floors, flips), and persists it as **one commit**: one
journal entry, one layout delta of O(batch) bytes. Batch size is
self-balancing (arrival rate × commit latency), capped by
`SQUEEZEFS_PUBLISH_COALESCE_MAX` (default 64; `1` = the pre-campaign
serialized per-op A/B lever). Panic containment: a dead pass fails its
queue LOUD (callers run the never-lossy ladder) and releases
leadership; idle conveyors retire from the map.

### 2.3 Composition

One coalesced batch ⇒ one delta entry: publish cycles per file drop
from every-4MiB to every-window, and entry bytes from O(map) to
O(batch). The two levers are independently killable
(`SQUEEZEFS_PUBLISH_COALESCE_MAX=1`, `SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN=0`)
and the remount-equivalence contract pins that all four combinations
persist identical layouts.

## 3. The crash-contract argument (what did NOT change)

- **One tx = one checksummed journal entry** — unchanged. A publish
  batch stages {layout delta | layout Put} + inode Put as ONE
  two-record transaction: whole-tx atomicity and torn-write immunity
  (§4.10) transfer verbatim. Zero journal/node format change beyond the
  new (feature-gated) delta payload class.
- **SIZE NEVER LEADS DATA (generic/795)** — *strengthened by
  construction*: the delta record carries its batch's size AND its map
  entries in ONE record inside ONE entry; no crash boundary can
  separate them. Pinned at fold level
  (`delta_size_and_entries_are_one_record`), through live remount +
  journal replay (`publish_coalesce_tests`), and under kill-9
  (`write_commit_crash_tests` invariant 2).
- **Relaxed streaming ACKs may coalesce — verified as today's
  contract**: a block whose publish waits in the conveyor queue is
  exactly as crash-exposed as a block awaiting the pre-campaign
  serialized merge queue (both are acked-but-not-yet-committed RAM
  custody under the writeback-cache contract; the never-lossy ladder
  owns transient failures either way). No new exposure class exists
  because the conveyor never ACKS an fsync/FLUSH before its ops'
  batches commit — the submitting future resolves only at terminal
  outcome, and fsync's flush merges ride the same conveyor ahead of its
  barrier.
- **fsync/FLUSH/RELEASE force-drain** — by construction (no parked
  window survives an fsync; there is no timer window at all). Pinned:
  `streaming_publishes_coalesce_and_fsync_is_durably_complete`
  (persisted-layout completeness on fsync return) + kill-9 invariant 1
  (every barrier-ACKED publish survives).
- **Never-lossy custody** — unchanged: batch save failure fans the
  error per-op (fencing classification preserved for
  `pipeline_disposition`); callers run the existing staging-fallback /
  stay-parked ladder.
- **Crash mid-window** — un-published blocks' data is unreachable but
  accounted (allocate→publish window, same class as the pre-campaign
  allocate→merge window, just wider): DMA'd offsets never named in a
  map return to free on remount; fsck C2/C3's allocation-epoch +
  in-flight-registry ladder already exempts/verifies the live window.
  Kill-9 soak: 10 rounds × mid-window deaths (74–150 publishes deep),
  acked-durability + size-map consistency + replay-twice digest
  equality + loud-mount-never, all green.
- **Replay** rides the same fold (one theorem): layout deltas replay
  through `apply_locked` → `fold_forward` / read-time
  `fold_newest_first` identically; canonical encoding keeps the
  §4.10 digest walk deterministic.

## 4. Sandbox economy evidence (in-tree contracts; SCOPING-ONLY numbers)

- `write_commit_economy_tests` (backend level, 256-block stream —
  windowed per-volume journal-ring deltas): pre-campaign
  ~1.2→7.6 KiB/publish growing with the map (the meta-plane audit
  table); post-campaign **flat ~230 B/publish** in every window
  (1 full + 255 delta commits — engagement exact), folded layout
  byte-equal to intent across clean-shutdown AND journal-replay
  remounts.
- `publish_coalesce_tests` (full write path, 64-block concurrent
  stream on the µs-commit sandbox with the pass-delay seam):
  64 publishes → **16 batches → 18 journal entries** (vs 66
  pre-campaign), delta commits 15/16, fsync-complete persisted layout.
  (The unconfigured test-process MEM_BUDGET clamps the pipeline to one
  block — the suite sets a flag budget; real mounts run GiB budgets.)
- `layout_delta_fold_tests`: fold equivalence (incl. randomized
  property over layout histories), compaction materialization,
  determinism, loud refusals.

## 5. Rig proof (TCP devsub, A-B-B-A vs e076db7)

**Instrument + substrate (stated per the standing rules):** elbencho
3.1-10 (dynamic), `--direct`, sync driver; `tests/wce_bracket.sh`
(committed) on the **nvmet-tcp localhost devsub** (`SQZ_DEVSUB_TRANSPORT=tcp`,
4 mds null_blk + 4 oss zram namespaces — the two-substrate rule's
fabric-sensitive venue); fresh format per run; kernel FUSE path; 16
threads × 512 MiB (2,048 × 4 MiB blocks/pass); **A-B-B-A + B-A ordering,
3 reps per binary, medians of 3**; A = tip `6785f0b` lineage
(`7cbe9f9` binary — later commits are harness/docs only), B = dev
`e076db7`. **Thermal validity:** the external governor held a constant
2.4 GHz through the entire bracket window (no frequency events in
`/tmp/thermal_governor.log` during the runs); the alternating order
guards residual drift. Raw CSVs + per-run elbencho/daemon logs:
`~/tmp/wce_rig{,_meta}/`.

**All dev-box numbers are venue-relative** (thermal-governed 2.0–2.4 GHz
box): the data plane saturates at ~800 MiB/s with `aqu` 10–13 and
4 MiB device requests on BOTH binaries (amp ≈ 1.00 everywhere), and the
RAM-backed meta journal commits in µs — so the field's ms-scale
commit-serialization wall does not bind here and **throughput parity ±
noise is the expected shape; the meta-plane economy columns are the
acceptance instruments**, exactly as §4's sandbox evidence.

### 5.1 Streaming rows (medians of 3)

| Row | Binary | MiB/s | META dev writes | META dev bytes | Journal entries | Journal bytes | J-bytes/block | META-bytes/block |
|---|---|---|---|---|---|---|---|---|
| fresh (2,048 blk) | B e076db7 | 762 | 1,294 | 12.19 MB | 2,677 | 4,371,673 | 2,152 | 5,998 |
| fresh | **A tip** | **826 (+8.4 %)** | **825** | **4.11 MB** | **2,428** | **578,621** | **285 (7.6× ↓)** | **2,022 (3.0× ↓)** |
| rewrite (2,048 blk) | B e076db7 | 771 | 1,368 | 18.18 MB | 2,637 | 8,064,711 | 3,938 | 8,876 |
| rewrite | **A tip** | **803 (+4.1 %)** | **797** | **3.72 MB** | **2,210** | **384,374** | **188 (21.0× ↓)** | **1,818 (4.9× ↓)** |
| sustained 60 s rewrite | B e076db7 | 802 (12,060 blk) | 7,955 | 108.8 MB | 15,522 | 47,623,136 | 3,949 | 9,023 |
| sustained | **A tip** | **770 (−4.0 %; 11,579 blk)** | **4,628** | **21.9 MB** | **12,677** | **2,237,703** | **193 (20.4× ↓)** | **1,887 (4.8× ↓)** |

- **The campaign claim lands:** journal bytes/block collapse **7.6×
  (fresh) / 21× (rewrite) / 20× (sustained)**; META-namespace device
  bytes/block collapse 3.0–4.9×; device writes/block 1.6–1.7× fewer.
  Rewrite/sustained collapse harder than fresh exactly as §1 predicts
  (pre-campaign rewrite re-serialized the FULL map per publish).
- **Engagement exact:** rewrite + sustained `layout_publish_batched_blocks`
  = 2,048/2,048 and 11,579 accounted; delta-commit share of batches
  98–100 % (fresh 1,798/1,830, rewrite 1,642/1,642). Fresh accounts
  2,032 = 127/file × 16 on the conveyor — the remaining 1/file is the
  staged-flush/promotion publish (`fuse_client.rs:7952`), which rides
  the direct primitive by design (not the streaming hot path).
- **Venue coalesce factor** (blocks/batch): 1.11 fresh / 1.25 rewrite /
  1.22 sustained — µs-scale commits mean batches barely need to form
  (the design's self-balancing law: batch size = arrival × commit
  latency). `meta_commit_group_size_median_lb` stayed 1 on both sides
  for the same reason. The deterministic batch-formation proof is the
  in-tree pass-delay-seam contract (§4: 64 publishes → 16 batches at
  5 ms latency); at the field's 19 ms cycles the same law predicts
  window-sized batches (§7 projections).
- **Sustained-state rule:** the 60 s `--infloop` rewrite row is flat
  (770 vs the one-shot 803, −4 %; block rate constant across the
  window). Throughput deltas (+8.4/+4.1/−4.0 %) are within this venue's
  run spread — parity, with the ceiling in the data plane (aqu 10–13)
  on both binaries.

### 5.2 Non-regression rows (medians of 3)

| Row | B e076db7 | A tip | Δ | Verdict |
|---|---|---|---|---|
| rand-4k `--direct` write, 30 s prefilled (IOPS) | 88,562 | 87,089 | −1.7 % (spreads overlap: A 81.3–88.4 k, B 84.6–88.7 k) | parity; `pub_blocks` 0 both — the W1 patch path, publishes correctly not engaged |
| seq read, prefilled (MiB/s) | 4,633 | 4,659 | +0.6 % | parity |
| create+unlink storm (8 thr × 250 dirs × 1×4 KiB file, create then delete): journal entries | 16,647 | 16,652 | **ratio 1.0003** (bytes +0.03 %) | G4-class economy non-regression PASS (≤ 1.05) |
| create+unlink ops/s | 9,444 | 9,309 | −1.4 % | parity |

Write-amplification columns (standing row requirement): DATA-namespace
amp 0.996–1.002 and `wareq` ≈ 4 MiB (block-sized device requests) on
every streaming row, both binaries — the levers touch only the meta
plane, as designed.

## 6. Gates

All on the campaign branch tip (final code tip `6785f0b`; the cargo
gate below ran from zero twice — after `7cbe9f9` and re-run after the
harness-script commit — identical results):

- `cargo clippy --all-targets --all-features -- -D warnings` PASS;
  `cargo fmt --check` PASS.
- `cargo test --all-features -- --test-threads=1` **full suite from
  zero: PASS** (1,580 tests, 0 failed; ~25 min on the throttled box).
  Campaign contracts included: `publish_coalesce_tests` (6 —
  engagement/economy, fsync durability, per-op fencing, remount
  equivalence, replay 795 law, **the new rewrite/displacement contract
  6**), `write_commit_crash_tests` (kill-9 soak ×10 rounds, mid-window
  SIGKILL anchored on the first acked publish + 5–50 ms jitter),
  `layout_delta_fold_tests` (8 incl. the randomized fold property),
  `write_commit_economy_tests` (2).
- `cargo doc --no-deps` builds; 4 pre-existing intra-doc-link warnings
  on dev-tip surfaces this branch does not touch (`ipc_host.rs`,
  `ipc_service.rs` ×2, `AdmissionGovernor`) — the two link warnings this
  campaign introduced were fixed (`432c2d0`).
- `cargo bench --benches -- --test` bench smoke PASS.
- Loom: no lock-free core changed (`ConveyorCore` is REUSED by lever 1,
  not modified — verified by diff); the conveyor-core models were re-run
  anyway (5/5 pass, `--cfg loom`, `LOOM_MAX_PREEMPTIONS=3`).
- **statfs ×10 loaded soak: 10/10 green** (real FUSE-over-io_uring
  mounts, 3 contracts/run; load = 8 CPU spinners + fsync-dd loop,
  87–93 s/run).
- **Preload gate leg 1 PASS** (sanctioned cdylib build, Issue-4 guard
  proof, passthrough battery, libaio lifecycle ×3 orderings) and **leg 2
  PASS** (mount parity + engagement, dup/close_range/lseek pins, fio +
  elbencho + fio-libaio verify, foreign-netns rendezvous, kill-9 soak ×5
  zero-residue, fork-kill-parent, direct-drive kill-9 soak — all OK).
- **pjdfstests (full, from zero, final tip): PASS** — 238 test files,
  8,798 tests, "All tests successful" (`sudo tests/run_pjdfstests.sh`,
  176 s wallclock, run alone per the serialized-heavy-phase rule).
- **full LTP (from zero, final tip): PASS** — 174 passed, 0 failed,
  0 broken, 9 skipped (TCONF — continue by design)
  (`sudo tests/run_ltp_syscalls.sh`, run alone, fail-fast armed and
  never fired).
- Suite order: cargo gate ×2 (from zero each) → statfs soak → preload
  legs → rig → pjdfstests → LTP; no heavy phases overlapped
  (thermal-event discipline).

## 7. Projected field impact (PROJECTIONS — the orchestrator runs the field verdict)

- Journal bytes/publish: 18.2 KiB mean → O(batch) (~0.3–1 KiB per
  BATCH of W blocks) ⇒ the per-volume journal byte ceiling stops
  binding at ~50× lower publish cost. (Venue-measured faces of the same
  collapse, §5.1: journal bytes/block 21× lower at rewrite, META device
  bytes/block 4.9× lower — on 512 MiB files; the field's 3 GiB files
  sit further up the O(map) curve, so the field collapse is LARGER.)
- Publish cycles/file: every-4MiB → every-window; at the field's 19 ms
  cycle and W≈8–16 batch formation, the per-file publish serialization
  term drops ~an order of magnitude — enough for 32 files to sustain
  the data plane's proven ~16+ GB/s (the depth pipeline stops parking
  39/40 blocks on commits).
- Counter-verifiable post-deploy: `layout_publish_batched_blocks ÷
  layout_publish_batches` (coalesce factor),
  `layout_delta_commits` vs `layout_full_commits` (delta engagement),
  `layout_delta_bytes` vs `meta_kv_journal_bytes_per_volume` (byte
  collapse), `meta_commit_group_size` (conveyor group formation), and
  the standing nvme0-vs-nvme2 iostat capture.
