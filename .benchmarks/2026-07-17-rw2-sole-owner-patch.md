# RW2 — W1 sole-owner extent patch: the scoreboard flip (2026-07-16/17)

**Charter**: PR RW2 of `docs/design-random-small-writes.md` (§5.1 W1, the
RW2 row) — the in-place, sub-block, LBA-aligned DMA for isolated small
overwrites of exclusively-owned, passthrough, whole-block-mapped striped
blocks, with the §5.1 clone/patch `fence(SeqCst)` protocol, the composed
two-word loom model, the flipped G-RW2 gate, and the measured acceptance
that removes the three `rand_write_4k` rows from the scoreboard allowlist
(G-RW5 part 1). USER SIGN-OFF was given for the architectural departure
(first deliberate in-place mutation of a mapped striped block).

## Provenance

| | |
|---|---|
| Tree | `perf/write-sole-owner-patch` off dev `94b8a01` (RED `f8d2c63`, impl `da9b16c`, gate flip `71c4fba`, this note + allowlist edit ride the closing commit); release binary md5 `6d94e7c79d67dba11aa373f215d10995` @ `71c4fba` |
| Box | the phase-1 box (25 online CPUs, 109 GiB RAM, nvme0n1 1.9T PC SN8000S, kernel 7.1.3-2-cachyos) |
| Rails | sandboxes under `/var/tmp/squeezefs_l1a` + `/var/tmp/squeezefs_vs_juicefs` and the fstests `/dev/shm` volumes (never `/mnt/squeezefs`, `/mnt/juicefs`, `~/tmp/nvme`); daemons in `systemd-run --user --scope` memcg cages; on-rail rows `taskset -c 0-15`; 3-poll quiet gate + Tctl ≥ 88 °C hard pause (observed 50.2–59.5 °C, every row `quiet`); kills by PID; elbencho 3.1-9; JuiceFS sandbox 1.5.0-dev+2026-07-14 (sqlite3 meta) |
| Measurement serialization | sole box owner; the three 📊 sessions (l1a rand grid → scoreboard → none concurrent) ran strictly one at a time; no other 📊 branch active |
| Artifacts | `/var/tmp/squeezefs_l1a/artifacts/20260716T221350Z` (rand grid, 12 rows: elbencho output, `.stats` before/after, diskstats snaps, honesty lines) + `/var/tmp/squeezefs_vs_juicefs/artifacts/20260716T222430Z` (full 3-regime scoreboard, 36 rows + counter/device evidence) |

## TDD evidence

- **RED** (`f8d2c63`): all 18 contracts failed on their assertions over
  inert scaffolding (M1 convention — never compilation): byte-exactness
  (`patch_writes == ops` read 0), `clone_cfr_vs_patch_storm`
  (`patch_ineligible_shared` never moved — the fence's CoW arm did not
  exist), the 8-bucket exclusion matrix (each decision-ledger counter
  0-vs-expected), both two-session crash audits, read-mid-patch coherence,
  EIO/verification paths, and the CLI-clone live-writer refusal (the
  pre-RW2 verb exited zero and never named the guard).
- **GREEN** (`da9b16c`): extent_patch_tests 18/18; the full cargo gate
  green — clippy `-D warnings`, fmt, `cargo test --all-features --
  --test-threads=1` (**93 test binaries, 0 failures**), doc, bench smoke;
  `tests/run_loom.sh` 27/27 models.
- **Gate flip** (`71c4fba`): the RW1 standing-RED
  `standing_red_g_rw2_device_byte_ledger_on_rand_write_shape` lost its
  `#[ignore]` (now `g_rw2_device_byte_ledger_on_rand_write_shape`,
  per-commit) and went green with the Issue-7 tripwires in the gate text.

## 1. The flipped G-RW2 gate (cargo tier, 64 KiB blocks, 768 ops)

```
read leg : spill_seed 0 (0 reads) | flush_seed 0 | write_path_seed 0      => 0.0x user   (was 8.0x RED)
write leg: spill_staging 0 | drain 0 | flush 0 | durable 0 | wt 0
           | patch 3,145,728 B                                            => 1.0x user   (was 10.7x RED)
patch    : patch_writes 768 (== ops) | edge_rmw_reads 0
per-op   : 4,096 B/op (was 76,544 B/op at RW1 — 18.7x per-op collapse at sandbox scale)
```

Gate clauses all inside bounds: write ≤ 4×, read ≤ 1×, `patch_writes ==
ops`, `patch_edge_rmw_reads == 0`.

## 2. The fence, as implemented (the §5.1 normative protocol)

- Patch side: `BlockAllocator::begin_patch_sole_owner`
  (`src/block_allocator.rs:185-189`) = `mark_incarnation_unstable` →
  `patch_clone_core::cross_word_fence()` → `refcount == 1` re-check; every
  back-off/error path `publish_block`s (re-stabilize, content unchanged).
  Call site: `SqueezefsFilesystem::try_sole_owner_patch`
  (`src/fuse_client.rs:3672` — fence step at `:3757-3765`), under the held `BLOCK_FLUSH_LOCKS`
  guard, after the lock-free overlay probes and the
  `is_whole_block_mapping` resolve of the authoritative cached map.
- Clone side: `BlockAllocator::pin_block_validated`
  (`src/block_allocator.rs:206-222`) = `increment_refcount` (pin-CAS) →
  `cross_word_fence()` → incarnation snapshot; `PinnedUnstable` ⇒ unpin +
  refetch under `INODE_META_LOCKS` + retry inside the existing
  `attempt >= 3` bounded loud refusal (`src/routing.rs` `clone_file`).
- The fence itself: `src/patch_clone_core.rs::cross_word_fence` —
  `fence(SeqCst)`, one dependency-free core `loom-models/` includes by
  `#[path]`, so the models check the shipped instruction sequence.

**Loom (⚙)**: `tests/run_loom.sh` green, 27 models, including the two new
ones — `patch_clone_composed_never_mutates_a_validated_pin` (the COMPOSED
two-word model: incarnation_core × refcount_core in ONE model, all
interleavings/orders, asserting ¬(pin-validated-against-the-pre-patch-word
∧ patch-proceeded) at generation precision, plus back-off re-stabilization
and refcount conservation) and
`incarnation_validated_fill_never_serves_mid_patch_bytes` (the R3
patch-interleaving fill case). **Falsification check performed**: with
`cross_word_fence` weakened to `fence(AcqRel)` the composed model FAILS on
the store-buffering outcome (loom explores it); restored to `SeqCst` it
passes — the fence is load-bearing, the model has teeth.

## 3. Targeted fstests (root, singles — the design's RW2 row set)

```
generic/074  63s ...  52s
generic/075  22s ...  18s
generic/112  22s ...  18s
generic/616  22s ...  21s
Ran: generic/074 generic/075 generic/112 generic/616
Passed all 4 tests
```

(4 MiB-block volumes — the fsx/overlay/punch families run with the patch
default-ON; whole-block shapes stay on the CoW write-through path by the
512 KiB cap, sub-block aligned overwrites patch.)

## 4. The RW1 rig ledger re-run — the amplification collapse (live mounts)

`tests/l1a_sweep.sh SHAPE=rand TVALUES="8 16" RAILS=on` — the RW1 baseline
knobs verbatim (16 GiB dataset over 16 files, `elbencho -w --rand -t {8,16}
-b 4k --iodepth 16 --direct --timelimit 30`, mb ∈ {12,256}, n=3, rig
armed, fresh volume per mb).

**IOPS medians (RW1 baseline → RW2):**

| cell | RW1 | RW2 | × |
|---|---:|---:|---:|
| mb12 t8 | 396 | **55,083** | 139× |
| mb12 t16 | 335 | **53,411** | 159× |
| mb256 t8 | 466 | **63,867** | 137× |
| mb256 t16 | 406 | **68,109** | 168× |

**Per-op device ledger** (diskstats adjudicate, counters attribute;
representative rows):

| row | ops | user | dev R | dev W | R amp | W amp |
|---|---:|---:|---:|---:|---:|---:|
| mb256.t16.r1 | 2,064,247 | 8,063 MiB | 6 MiB | 14,406 MiB | **0.001×** | **1.787×** |
| mb256.t16.r3 | 2,043,440 | 7,982 MiB | 8 MiB | 25,479 MiB | 0.001× | 3.192× |
| mb12.t8.r1 | 1,652,769 | 6,456 MiB | 193 MiB | 8,339 MiB | 0.030× | 1.292× |

vs the RW1 baseline's **524–551× reads / 1,453–1,480× writes (7.7–7.9
MiB/op)**: per-op device cost is now 4–13 KiB/op — the §1.2 pipeline is
dead on the patch shape. Decision-ledger truth on the same rows:
`patch_writes ≈ ops` (2.06 M / 2.04 M / 1.65 M), `patch_edge_rmw_reads = 0`
everywhere, stray fallbacks fully attributed (overlay 0–427 = the untimed
seq-prep's parked-tail residue at row start; adjacent ≤ 1; every other
bucket 0), `get_obj` ≤ 25/row, `meta_kv_journal_entries` ≤ 32/row (mount +
prep tail — the patch path itself commits nothing). The mb256.t16.r3
3.19× write-amp outlier is same-row background traffic (the harness's
per-row diskstats window includes the prior row's writeback drain), still
inside the ≤ 4× gate.

**FIND-L1-A / convoy signature (G-RW1 deferral clause)**: the rig emitted

```
SIGNATURE rail=on shape=rand t16/t8@mb256=1.07 mb256/mb12@t16=1.28 blw_ms_tail@t16=95131 @t8=26694 verdict=not-convoy-shaped
```

t16 ≥ t8 (1.07) and mb256 > mb12 (1.28) — both L1 boundaries absent, as in
the RW1 baseline session (which had already found the −25 % convoy
non-reproducing on this tree and directed RW3 to start from a re-repro
attempt). **The FIND-L1-A deferral caveat is documented moot**: the t16
rand rows are not convoy-shaped, so G-RW1 is adjudicated FINAL here, no
post-RW3 re-measurement owed. (The `block_lock_wait` ms-tails at mb256 are
the patch pipeline's own device-latency shadow under 16×16 in-flight
DMAs — they scale with admission, not against writers, and the throughput
rises with them.)

## 5. The scoreboard — G-RW1 flipped, G-RW3 spot rows unmoved, gate GREEN

Full 3-regime × 6-workload session,
`SQUEEZEFS_VS_ALLOW_LOSS="R1.seq_write_1m,R3.seq_write_1m"` (the shrunk
ledger — the three rand_write rows REMOVED by this PR), exit **0 = GATE
GREEN**:

| Row | JFS | SQZ | SQZ/JFS | Verdict |
|---|---:|---:|---:|:--:|
| **R1.rand_write_4k** | 4,638 | **61,510 IOPS** | **13.26×** | **W** |
| **R2.rand_write_4k** | 4,248 | **63,870 IOPS** | **15.04×** | **W** |
| **R3.rand_write_4k** | 4,574 | **66,657 IOPS** | **14.57×** | **W** |
| R1.seq_write_1m | 10,760 | 4,296 MiB/s | 0.40× | L (allowed — ACK-semantics artifact) |
| R2.seq_write_1m (device-true class) | 2,284 | 4,278 MiB/s | 1.87× | W |
| R3.seq_write_1m | 10,290 | 4,487 MiB/s | 0.44× | L (allowed — ACK-semantics artifact) |
| R{1,2,3}.seq_read_1m | 6,645/6,581/6,577 | 6,652/6,580/6,584 | 1.00× | TIE |
| R{1,2,3}.rand_read_4k | 120,645/84,061/125,787 | 236,043/238,218/268,580 | 1.96×/2.83×/2.14× | W |
| R{1,2,3}.stat_storm | | 339,025/337,673/348,730 | 2.88×/2.84×/2.95× | W |
| R{1,2,3}.del_storm | | 32,422/33,348/36,984 | 9.49×/9.47×/10.95× | W |

- **G-RW1 (the gate): PASSED FINAL** — the W verdict in every regime,
  13.3–15.0× JuiceFS vs the ≥ 1.05× gate (≈ ≥ 4.7 k floor → measured
  61.5–66.7 k). The 10 k stretch is exceeded 6×; the §4 20 k arithmetic
  expectation is exceeded 3× (no < 10 k attribution clause owed). The
  FIND-L1-A caveat is moot per §4 above.
- **G-RW2 on the scoreboard row itself**: R3.rand_write dev_r/s = 548
  (9 MiB/s ≈ 0.03× user), dev_w = 311 MiB/s vs ~260 MiB/s user ≈ **1.19×**
  — the Loss-2 protocol's own instruments confirm the ledger;
  `get_objΔ = 24` on a 2 M-op row.
- **G-RW3 spot rows (unmoved)**: device-true seq-write class R2 1.87× W
  (baseline 1.65× W, same 4.3–4.5 GiB/s device class — 4,278 MiB/s);
  seq_read TIE 1.00× in all regimes (baseline TIE); rand_read
  1.96–2.83× W (baseline 2.07–2.84×); stat 2.84–2.95× (baseline 2.7–2.9×);
  del 9.47–10.95× (baseline 9.3–10.3×); the two seq-write ACK-semantics
  rows keep their allowed-loss class (0.40×/0.44×, baseline class).
- **Allowlist diff (G-RW5 part 1, riding this merge)**:
  `R1.rand_write_4k, R2.rand_write_4k, R3.rand_write_4k` **deleted**;
  remainder = `R1.seq_write_1m, R3.seq_write_1m` (the two ACK-semantics
  artifact rows — RW6's harness durability mode would take them to ∅).
  The posture is now pinned in `tests/run_vs_juicefs.sh`'s header
  (STANDING ALLOWLIST block).

## 6. What landed (mechanism inventory)

- **Patch route** (`try_sole_owner_patch`, `src/fuse_client.rs`): the
  6-predicate decision (first-fail ledger: adjacent → oversize [len>cap /
  spans blocks / extending] → unaligned at request shape; overlay →
  transform → unmapped/decorated → shared under the held block lock) →
  mark-unstable → `fence(SeqCst)` → refcount re-check → severed pooled
  aligned payload (BUFFER_POOL backing, `WriteData::Aligned` by
  construction, §5.4's 1 copy + 1 DMA) → `publish_block` → 4-arm
  `purge_block_key` + whole-file LRU drops → ACK. Zero meta, zero staging,
  zero allocation. DMA/verify failure = EIO for exactly that write,
  re-stabilized + purged.
- **`is_whole_block_mapping()`** (`src/routing.rs`) — THE single
  predicate-1 source (Issue-19 polarity: the undecorated 2-part form is
  the eligible population; `exact == true` marks the ineligible decorated
  form). The byte-exactness red case ran first against inert scaffolding
  and pinned the polarity forever.
- **Clone validate-after-pin** (`clone_file` + `pin_block_validated`) with
  the bounded `attempt >= 3` EBUSY-class loud refusal (accepted, Issue 16);
  `clone_cfr_vs_patch_storm` drives both interleaving orders in-process
  (CFR whole-file arm), byte-audits both files post-race AND post-remount,
  and watchdogs the never-hang contract.
- **Lock-free staged-existence probe** (Issue 10): a conservative-present
  `scc` occupancy index in `NvmeStaging` (indexed before the ring write,
  un-indexed after the ring removal, seeded from the recovered ring at
  mount) — the patch hot path takes no `spawn_blocking`/shard-lock hop.
- **Stream-adjacency guard**: per-ino `last_write_end` word (`scc`, one
  relaxed swap per striped write, dropped at inode reclaim) — seq streams
  keep the whole-block write-through economy (`patch_ineligible_adjacent`).
- **Knob + stats**: `SQUEEZEFS_PATCH_MAX_BYTES` (default 512 KiB, `0` =
  A/B lever); stats families `patch_writes`, `patch_write_bytes`,
  `patch_edge_rmw_reads` (**0 by definition in v1** — gate clause),
  `patch_ineligible_{unmapped,decorated,unaligned,overlay,shared,
  transform,adjacent,oversize}`, `patch_dma_errors` — all on the stats
  inode.
- **CLI-clone live-writer refusal** (D0): `squeezefs clone` now opens the
  sqmeta volumes under the single-writer guard for the clone's duration; a
  live writer refuses loudly (enforced + tested via binary spawn).
- **refcount_core::peek** — the read-only refcount accessor at the core
  level (loom-included).
- **Crash contract**: two-session kill-9-equivalent audits pin the v1
  aligned-only blast radius — foreign bytes NEVER perturbed; the
  app-written window old-or-new; failed-DMA crash leaves the old block
  fully intact. The item-B `crash_inside_window_leaves_old_block_intact`
  stays true verbatim on the accumulation path (its binary pins the patch
  OFF and points here for the patched twin).

## 7. Findings (recorded, out of this PR's scope)

- **FIND-RW2-A (pre-existing, latent)** — **FIXED in RW4** (`76ee762`,
  `.benchmarks/2026-07-17-rw4-extent-overlay.md`: the device-fetch funnel
  decodes decorated mappings; incarnation tracking keys on the cleaned
  base offset; pinned by `fold_seeds_decorated_promoted_mapping`):
  materializing a deferred RMW seed
  for a striped block whose mapping is DECORATED (`bk:off:len`) fails
  `Io(InvalidData "Invalid block offset")` — `fetch_seed_image →
  get_block_for_index → BackendRouter::read_block` parses the decorated
  string as a raw key. Verified identical with `SQUEEZEFS_PATCH_MAX_BYTES=0`
  (i.e. on pre-RW2 behavior); the natural producers (promoted-staged files
  grown striped without rewriting block 0) can reach it. The RW2 decorated
  exclusion test uses a block-complete overwrite (write-through fallback,
  no deferred seed) and documents this; a fix belongs to the read/flush
  path owner (suggest: decode via `clean_block_key`/`parse_block_mapping`
  at the fetch layer).
- **FIND-RW2-B (new interaction, bounded)**: because patched keys never
  displace, per-key read-side heuristic state (ghost table,
  `EscalationCooldown`) now outlives overwrites; a hybrid O_DIRECT
  second-touch re-admission inside a cooldown window (~32–64 s) is
  suppressed where the CoW path's fresh key would have re-admitted.
  Correctness unaffected (ranged device reads serve; the patch purges all
  four tiers before ACK); scoreboard granularity shows no read regression
  (rand_read W in all regimes, above). The G-RW3 mixed rand-R/W + hybrid
  warm rows at RW5's full sweep are the standing measurement; if live
  mixed shapes show tier-warmth loss, the lever is a cooldown/ghost reset
  in the patch's purge (one line, read-path owner's call).
- **Suite posture**: the accumulation-pipeline / CoW-displacement /
  parked-drain machinery tests (rand_write_amp ×3, rig-off, the item-B
  binary, data_path reused-key, hot-block dead-key, reused-key ABA,
  mem_budget advisory, hybrid 091) now pin `SQUEEZEFS_PATCH_MAX_BYTES=0`
  with rationale comments — that machinery still owns every
  patch-ineligible shape; `tests/extent_patch_tests.rs` owns the patched
  twins. Harness constructors reset the knob so pins never leak.

## 8. Gate adjudication summary

| Gate | Verdict | Evidence |
|---|---|---|
| G-RW1 (≥ 1.05× JFS every regime; ≈ ≥ 4.7 k floor) | **PASSED FINAL** — 13.26× / 15.04× / 14.57× (61.5–66.7 k IOPS); stretch 10 k+ exceeded; L1-A deferral moot (not-convoy-shaped) | §5, §4 |
| G-RW2 (≤ 4× W, ≤ 1× R, `patch_writes ≈ ops`, `edge_rmw = 0`) | **PASSED** — cargo gate 1.0×/0.0×; live diskstats 1.19–3.19× W, ≤ 0.03× R; tripwires exact | §1, §4, §5 |
| G-RW3 (zero regression elsewhere) | **spot rows PASSED** — device-true seq W class, seq/rand read, stat/del, QUICK-family singles (074/075/112/616), crash suites, loom; the full sweep remains RW5's | §3, §5 |
| G-RW5 part 1 (allowlist shrinks) | **DONE** — 3 rand_write rows deleted; remainder = the two seq ACK rows; gate run GREEN under the shrunk list | §5 |

**The scoreboard flipped at RW2, as designed.** RW3 (FIND-L1-A forensics —
starting from the re-repro attempt RW1 directed), RW4 (W2 extent overlay,
phase 2) and RW5 (closing report + the one full sweep) follow.
