# 2026-08-02 — Dynamic meta routing: derived virtual width, measured

**Campaign:** `feat/dynamic-meta-routing` (off dev `29e964d`).
**Charter:** direct user ruling (2026-08-02): the frozen, user-chosen
routing width is "a horrible / restrictive design decision... it needs
to be dynamic." Forward-only law applied (old formats refuse loud,
reformat required); no-fixed-constants law applied (W and the mint
spread are stated derivations, not knobs).
**Design:** `docs/design-dynamic-meta-routing.md` (Rev 2). Architecture
**A — large derived virtual W** adjudicated over B (extendible hashing)
on counted grounds: B requires a W-independent record-key encoding (the
local-ino derivation `local = (ino−2)/W + 2` is W-dependent) — a
keyspace-layer rewrite plus O(volume)-scan slot migration — while A is
encoding + policy only, with the entire VL5b migration engine, cutover
gate, flip protocol, and crash windows untouched.

**Substrate/instrument statement (house law):** local file-backed
MetaLV sandboxes (`tempfile` + 256 MiB file volumes), unprivileged;
in-process walls via `std::time::Instant`; structural probes via
`rustc -O` standalone (medians of 1000 iterations); end-to-end walls
via the `idle_cost_probe` test (debug build — conservative upper
bounds). No cluster work (charter). Not a perf-claim campaign: the
numbers below are IDLE-COST adjudication evidence, not throughput rows,
so no sustained-state row applies.

---

## 1. What shipped

| Piece | Where |
|---|---|
| `DERIVED_ROUTING_WIDTH = 2^16` (the u16 slot-id namespace — every format freezes it into its stamps; mounts route over the STORED width) | `src/meta_backend/mod.rs` |
| `SlotSet` stride runs `(start u16, stride u16, count u32)` — the at-rest encoding | `src/meta_backend/kv/slot_set.rs` |
| Stamp wire v3 (runs + `native_plus1` u32 + cursors), `STAMP_MAX_RUNS = 128` / `STAMP_MAX_CURSORS = 256` replacing the retired 64-hosted-slot cap + `W ≤ 64×V` format bound | `src/meta_backend/kv/checkpoint.rs` |
| `KV_DYNAMIC_ROUTING` = incompat **bit 6**, presence-REQUIRED at decode (the `NODE_SEQ_WATERMARK` pattern) | `src/meta_backend/kv/superblock.rs` |
| Mint spread: `MINT_SPREAD = 64` (= `STAMP_MAX_CURSORS/4`) rotor per volume; gated ops pick-then-gate-then-mint (`pick_mint_slot` → `allocate_local_ino_in_slot`) | `src/meta_backend/mod.rs` |
| `format_v3` = single-member dynamic set (synthesized stamp; set-wide hash seed always) | `src/meta_backend/kv/builder.rs` |
| `--meta-slots` hard error naming `volume add-meta --take-slots` / `migrate-meta-slot`; format prints the derived-width story | `src/main.rs` |
| FormatConfig mirror `meta_slot_map` (O(W)) → `meta_slot_runs` (O(runs)) | `src/lib.rs`, `src/config_ops.rs`, `src/meta_backend/slot_migration.rs` |
| `--take-slots` census probes only record-bearing slots (native + cursor-bearing — virgin hosted slots are empty by construction) | `src/config_ops.rs` |
| Stats: `meta_routing_width`, `meta_slot_mint_spread`, `meta_slot_stamp_{runs,cursors}_max` (encoding-budget pressure gauges) | `src/fuse_client.rs` |
| Contract battery (17 tests incl. the ignored probe) + reworked `meta_slot_tests`/`meta_slot_migration_tests` + rig leg 18 | `tests/dynamic_meta_routing_tests.rs`, `tests/run_volume_lifecycle.sh` |
| Criterion group `dynamic_meta_routing` (standing idle-cost regression instrument) | `benches/high_concurrency_bench.rs` |

## 2. Idle-cost measurements at the chosen W (the A-adjudication)

### 2.1 Structural probes (rustc -O standalone, this box, medians of 1000)

W = 65536, V ∈ {1, 2, 4, 8}:

| Pass | V=1 | V=2 | V=4 | V=8 |
|---|---|---|---|---|
| format plan expansion (O(W)) | 186 µs | 167 µs | 165 µs | 166 µs |
| discovery claim resolution (O(W)) | 104 µs | 100 µs | 131 µs | 126 µs |
| validate + native derivation (O(W)) | 31 µs | 24 µs | 24 µs | 24 µs |
| `route_ino` round trip (arith + table) | 1.1 ns/op | 1.0 | 1.0 | 1.1 |

### 2.2 End-to-end (idle_cost_probe, file-backed sandbox, debug build)

| Shape | format | discover | open (D0-guarded, replay) | at-rest stamp | route table RAM |
|---|---|---|---|---|---|
| V=1, W=65536 | 7.0 ms | 7.8 ms | 20.9 ms | **48 B** | 512 KiB/mount |
| V=2, W=65536 | 7.6 ms | 6.7 ms | 27.9 ms | 48 B/member | 512 KiB/mount |

At-rest stamp bytes: 48 B fresh (one 8-B stride run); worst legal stamp
(128 runs + 256 cursors) = 3 624 B vs the 3 953-B ledger-slot budget —
`encode_slot` boundary pinned at cap / cap+1
(`test_encoding_budget_caps_encode_at_cap_refuse_past_it`).

**Verdict:** per-slot idle cost aggregates to O(1) per volume on disk,
~150 µs of one-time O(W) passes and 512 KiB RAM per mount. Amortized to
irrelevance — the §Phase-1 requirement met; A stands without pause.

## 3. Migration-granularity behavior

The pre-campaign trap: only MINT slots carry records, and each volume
had exactly one — so a huge W without a mint-policy change would have
been cosmetic (a fresh V-volume set had V loaded slots; the movable
unit was a whole volume's load). Shipped behavior:

- Minting rotates across `min(MINT_SPREAD, hosted)` = 64 slots/volume
  (`test_mint_spread_rotates_across_the_mint_set_and_survives_remount`:
  256 allocations land in exactly 64 distinct slots; globals unique;
  remount reseeds cursors with zero collision).
- Granularity: **1/64 of any volume's load per migrated slot, from
  birth, at any volume count** — including V = 1, the shape that could
  never grow at all before (rig leg 18 grows a single-volume default
  format to two members by `add-meta --take-slots 8` with byte identity
  + st_ino stability + clean fsck).
- Ino stability across a LOADED slot migration + remount pinned at the
  derived width
  (`test_created_inos_stable_across_slot_migration_and_remount`).
- Stride-doubling growth (V → 2V takes every second slot of each
  donor's run) keeps stamps at O(1) runs — B's prefix split expressed
  in modulo space on unchanged machinery (`slot_set.rs` unit pin).

## 4. Refusal shapes (forward-only)

| Shape | Refusal (pinned) |
|---|---|
| v3 volume WITHOUT bit 6 (any frozen-width-era format — legacy identity or `--meta-slots`) | "formatted with a frozen routing width (pre-dynamic-meta-routing) — no longer supported ... reformat required" at the superblock gate, before any ledger read (`test_bit6_absent_v3_volume_refuses_loud_with_reformat_guidance`) |
| Old binaries vs new volumes | bit 6 ∉ any prior `FEATURES_INCOMPAT_KNOWN` mask (non-intersection pinned; pre-VL5a masks refuse on bit 2, pre-campaign masks on bit 6 — `test_incompat_refusal_ladder_and_stampless_defense`) |
| `format --meta-slots N` | hard error naming the derivation + the growth verbs, BEFORE any validation/destructive step (`test_meta_slots_flag_refuses_naming_successor`, rig leg 18) |
| Stampless bit-6 volume (torn format / foreign ledger) | loud discovery refusal naming repair-set/reformat — the legacy implicit-identity arm is deleted |
| Two independently-formatted volumes listed as one set | foreign-set refusal (each `format_v3` is its own single-member set) |
| Width past the slot-id namespace | `plan_meta_slot_set_with_width` refuses naming the derivation |

## 5. Gates run (this campaign, on this branch)

The release-gate campaign occupied the box through the design + code
phases (all work `nice -n 15`, `CARGO_BUILD_JOBS=8`, targeted binaries
only); it freed the box before the acceptance gates, which then ran in
full:

- **The full cargo gate — GREEN from zero** (counted-run discipline: an
  earlier pass caught exactly one stale contract —
  `staging_generation_tests::volume_set_generation_identity_contracts`,
  the retired legacy ordered-join shape, re-pinned to the stronger
  foreign-set refusal — and the gate was restarted from zero post-fix):
  `cargo clippy --all-targets --all-features -- -D warnings` clean;
  `cargo fmt --check` clean; `cargo test --all-features --
  --test-threads=1` — the ENTIRE suite, zero failures; `cargo doc
  --no-deps` (no unresolved links introduced; three pre-existing
  private-item warnings untouched); `cargo bench --benches -- --test`
  smoke green (incl. the new `dynamic_meta_routing` group).
- **`tests/run_volume_lifecycle.sh` — ALL 18 LEGS PASSED** (release
  binary, unprivileged fuse, LOOPS=3): the VL5 legs at the derived
  width — leg 9 add-meta (manifest byte-identical, inos stable, old URI
  refused), leg 10 remove-meta, leg 11 kill-9-during-migration ×3
  (online cutover window 1 ms), leg 17b migrate+drain concurrent — and
  the NEW **leg 18: a single-meta-volume default format grew to two
  members by `add-meta --take-slots 8`** (the previously-impossible
  shape): derived-width format banner asserted, `--meta-slots` hard
  error asserted, manifest + st_ino intact across the grow + remount,
  post-grow fsck findings 0.
- Targeted suites during development (serial where process-global
  counters demand): `dynamic_meta_routing_tests` (16 + probe),
  `meta_slot_tests` (17), `meta_slot_migration_tests` (16),
  `meta_plane_distribution_tests`, `interaction_tests` (16),
  `volume_lifecycle_tests` (19), `fsck_tests`/`fsck_repair_tests`,
  `kv_backend_tests` (34), `conveyor_tests`, `crash_contract_tests`
  (25), `meta_dlm_stripe_tests`, `meta_entry_economy_tests`,
  `job_fabric_tests`, `defrag_tests`, `placement_tests`,
  `write_commit_economy_tests`, `meta_write_economy_audit_tests`,
  `mount_writer_guard_tests` — all green.

Branch state: **ready to merge** (`feat/dynamic-meta-routing`, ff-only
onto dev after review).

## 6. Operator story (the deliverable sentence)

**Format anywhere, grow forever, no knobs.** Every format freezes the
derived 65536-slot virtual width and spreads minting so any volume's
metadata is divisible into ≥ 64 movable slices from birth; growth is
`volume add-meta --take-slots …` (offline) or `migrate-meta-slot`
(online), to a ceiling of 65536 metadata volumes nothing will reach.
