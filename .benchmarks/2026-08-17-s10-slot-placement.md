# 2026-08-17 — Rung 14: S10 CLIENT-OWNED-SLOT PLACEMENT (KD-MW-6 — the formal tar-x gate, adjudicated)

**Branch** `feat/s10-slot-placement` (worktree off dev `633b2a08`).
Charter: `docs/design-full-multi-writer.md` §8.2 lever 2 + PR row 14 —
*"`src/meta_ship/router.rs` placement hint; policy over
`mint_redirects`/slot migration — Client-owned-slot placement; **gate:
`tar -x` recovered to ≤1.10× S0 baseline at netem 250 µs** (or the honest
product statement lands in operations.md + rc-manifest)"* — consuming
rung 13's decomposition (`.benchmarks/2026-08-17-s10-update-intents.md`)
as the gate input and rung 11's valve arithmetic as the anti-thrash law.

**Red-first discipline.** The whole suite
(`tests/mw_slot_placement_tests.rs`, 8 arms) landed first and fails to
compile at its commit (`622717f1` — "no `placement` in `meta_ship`",
captured before a line of implementation existed).

---

## The verdict first (the rung's charter allows exactly two outcomes)

**GATE NOT MET — the honest product statement governs**, and it landed in
`docs/operations.md` §Metadata function shipping +
`docs/rc-manifest.md` (the S10 row) per the charter's explicit
alternative. The measured row (below) reads **6.73× of S0** against the
≤1.10× gate, and the miss is **architecture, not tuning**:

* On every fleet the product can mount today, exactly ONE node holds the
  D0 `writer_claim` on every metadata volume of a set. A co-writer opens
  every volume **read-only** (`KvMetaBackend::open_co_writer`, the
  `CoWriterMount` latch) — it can commit NOTHING locally, so **no slot it
  could own exists**, the placement policy's candidate inversion
  (`OwnerMap::volumes_owned_by`) is empty by construction, and no slot
  migration can make its verbs local.
* The ON-arm residual (~11.7 S8 verbs + ~2.8 publish verbs per entry with
  the create/utime plane already fully local — rung 13's decomposition,
  reproduced here within noise) is per-entry kernel reads
  (census-positive lookups/getattrs) + per-file custody/lease ceremony +
  the inline-publish stream — **none of which slot placement can localize
  for a client that owns no volume**.
* Spec §6.10 R1's own fallback is therefore the product statement,
  verbatim: *"remote clients are throughput-oriented; latency-sensitive
  metadata work runs on the owner."*

**What makes the gate meetable later** (named, not hidden): per-volume
claim admission — a client that holds the D0 claim on ≥ 1 volume of a
shared set (the §6.10 R4 *fleet-of-authorities* recipe). The placement
machinery for that day is **landed, dark, and pinned in-process** against
exactly that shape (an armed map naming the client as a volume's owner
drives the REAL online migration engine end-to-end in cargo).

## THE GATE TABLE (measured)

Venue: `mw_fleet.sh create N=2 --multi-writer --cowriters=1`
(instance-suffixed tcp devsub `r14pl`, nvmet-tcp 127.0.0.1:54145,
`SQZ_MWFLEET_MW_PORT=54193`, `SQZ_MWFLEET_OSS_GB=24`), release binary at
`52bc91bf`, kernel 7.1.6-1-cachyos-sqz, quiet-gated (no cargo, loadavg
≤ 4). Instrument: the rung-9/13 instrument VERBATIM — **real linux-src
`fs/` tree (2,384 entries, 49 MB)**, netns co-writer at netem
125 µs/end = **250 µs wire RTT**, vs the **authority-LOCAL S0 baseline on
the same venue/binary/tarball/store**, A-B-B-A (pl, local, local, pl).
Levers = the shipped defaults (delegation + intents + placement all ON).
Leg: `tests/run_mw_matrix.sh s10-placement-tarx` (new).

| Arm | wall s | entries/s | intent mints | S8 ships | publish ships | placement mints (owner) | wire verbs/entry |
|---|---|---|---|---|---|---|---|
| pl-on-1 | 14.83 | 161 | 2,286 | 28,084 | 6,660 | **1,330** | 14.57 |
| local-1 | 2.16 | **1,105** | — | 0 | 0 | 0 | 0 |
| local-2 | 2.26 | **1,057** | — | 0 | 0 | 0 | 0 |
| pl-on-2 | 14.91 | 160 | 2,286 | 28,084 | 6,467 | **1,330** | 14.49 |

**gate: co-writer 14.87 s vs local 2.21 s → 6.73× of S0 (gate ≤ 1.10×):
GATE NOT MET.**

Honest reading, order-independent (A-B-B-A agrees to < 1 % on both arm
pairs):

* **Engagement is exact and the placement ledger CLOSES to the op**:
  `meta_ship_placement_client_slot_mints` Δ = 1,330 per arm = 98
  grant-earning shipped creates (2,384 − 2,286 local mints) + 1,232
  supply reservations (`meta_ship_placement_supply_events` Δ = 1,232) —
  every mint executed FOR the client landed in its dedicated slot;
  `rotor_fallbacks` = 0.
* **The migration half is structurally dark, proven live**:
  `migration_candidates` = 0 and `migrations_triggered` = 0 across the
  whole sweep (asserted by the leg — the one-authority topology's
  posture), while `sustain_evidence` moved (the policy observed the
  sustained run and found no candidate volume, exactly as designed).
* The ON arms reproduce rung 13's row within noise (161/160 vs 161–162
  e/s; 14.5 vs 14.38 verbs/entry): **placement adds zero wire change on
  this topology by construction** — its value here is the migratable
  per-client unit + the formal gate adjudication; its recovery value
  waits on the fleet-of-authorities admission.
* The local S0 arms (1,057–1,105 e/s) are same-venue/same-binary — the
  charter's comparison — and sit slightly above rung 9's 995 e/s
  (different day/binary; the gate ratio uses only same-run arms).
* fsck oracle after the sweep: **findings: 0 (clean)**,
  `meta_kv_block_refs_drift` = 0, `owner_panics` = 0.

## What landed

* **The mint-targeting hint** (`src/meta_ship/placement.rs`,
  consulted at the ONE mint funnel —
  `RoutedMetaBackend::pick_mint_slot`): a mint executing FOR a shipping
  client (the owner-side `SHIP_CLIENT` task-local — the same
  deep-in-backend position the rung-12 mutation gate reads; **zero wire
  change**, it covers shipped creates AND intent-supply reservations with
  one hook) lands in a slot **dedicated to that client**: chosen OUTSIDE
  the volume's mint set (the owner's rotor never interleaves into it —
  the migratable unit stays clean), stable per `(client, volume)`
  (cached; re-derived only when a migration moves the slot — the policy
  WORKING), deterministic hash + linear-probe past other clients' slots
  (distinctness is structural, never probabilistic). Composes **under**
  `constrain_mint_volume` (one appender per volume, §6.2 items 2/3/4),
  never above it. The supply REFILL reservation now runs inside the
  client's `SHIP_CLIENT` scope (it was outside — refills would have
  ridden the rotor), and this rung discharges rung 13's residual 5
  ("supply placement is single-slot ... row 14's lever").
* **The migration policy** (`note_supply_event`, hooks at UPDATE-grant
  issuance + supply refill — control-plane rate, zero hot-path cost):
  evidence = a run of supply events, threshold = the rung-11
  pattern-vs-coincidence constant (3); at a sustained run the
  **ownership-map inversion** (`OwnerMap::volumes_owned_by`) asks whether
  the client OWNS a volume — empty ⇒ dark (today, always); named ⇒ the
  hot set (the client's dedicated slots + its granted directories'
  slots) migrates toward the client's volume, one slot per trigger, one
  migration in flight process-wide, through the installed executor.
* **The executor is the EXISTING engine** —
  `authority_migration_executor` = the online
  `slot_migration::migrate_slot` + `rearm_ownership` at completion
  (discharging `owners.rs`'s stated migration-while-armed assumption: a
  slot move changes the derived local slot set, so the map republishes at
  cutover exactly as the routing table does). Installed by
  `multi_writer::arm_multi_writer`, uninstalled at disarm.
* **The valve** (never-thrash, the acceptance's ping-pong law): per-slot
  migration episodes inside the thrash window count cycles; engaged AT
  the rung-11 constant (the storm table's shape — the Nth move still
  issues, later attempts hold), cooldown = 8× window
  (`recall_cooldown_from` reused), window = the membership lease TTL
  (`recall_lease_ttl`, now `pub(crate)`) — a slot moving twice inside one
  lease period cycles faster than clients re-home custody; slower
  alternation is priced as legitimate re-placement. **The valve has no
  knob** (structural — the rung-11 law).
* **Era fencing**: `placement::fence_client` rides rung 13's
  `note_client_incarnation` (epoch change) AND the recall-deadline
  expiry fence — a zombie incarnation's assignments + evidence die, so
  its half-run can never compose with its successor's into a trigger.
  Placement state also dies wholesale at `disarm_ownership`.
* **`SQUEEZEFS_SLOT_PLACEMENT`** (ENG-10 registry; `Kind::Bool`, static
  default on, read only when the ownership plane is armed — the
  `SQUEEZEFS_DELEGATION` lever form; `=0` is the A/B control, pinned
  dark).
* **Stats**: the `meta_ship_placement` object —
  `client_slot_mints` (engagement; closes to shipped-creates +
  supply-events), `client_slots` (gauge), `rotor_fallbacks`,
  `supply_events`, `sustain_evidence` (gauge — dies with a fence),
  `migration_candidates` (structurally 0 on every one-authority fleet),
  `migrations_{triggered,completed,failed}`, `thrash_demotions`,
  `valve_holds`, `fences`.
* **Rig**: the `s10-placement-tarx` leg (the FORMAL gate row: A-B-B-A,
  REQUIRES the real linux-src tree — a synthesized all-inline tree
  cannot exercise the oracles; publishes the table + verdict either way,
  exits nonzero only on INVALID rows/oracle; asserts the dark posture
  live), plus a rig fix: the matrix's sudo re-exec now preserves
  `SQZ_MWMATRIX_*` (TAR_SRC was silently dropped unless invoked as root
  directly — a real-tree run downgraded to the synthesized fallback).

## The pins (all 8 green; red at `622717f1`)

| Arm | Law |
|---|---|
| `dark_posture_an_unarmed_mount_has_no_placement_state` | unarmed mounts: rotor untouched, every gauge 0 |
| `the_lever_off_control_is_dark_on_an_armed_mount` | `=0` (force-off seam): the supply rides the rotor INSIDE the mint set, no state forms |
| `a_placement_armed_clients_mints_land_in_its_dedicated_slot_stably` | ONE dedicated slot across ≥ 2 supply reservations, outside the mint set, never slot 0; every supply ino routes to the granted dir's volume (the mint constraint / `mint_redirects` machinery composed UNDER, never bypassed) |
| `two_placement_armed_clients_get_distinct_dedicated_slots` | distinctness is structural (probe past taken slots) |
| `the_migration_policy_engages_on_sustained_client_concentration` | the in-process fleet-of-authorities shape (map names the client as volume 1's owner): 3 supply events → candidate found → the REAL `migrate_slot` engine moves the client's slot to volume 1 → its inos route `Ship(client)` — the S8 owner path |
| `the_policy_stays_dark_when_no_client_owned_volume_exists` | the shipped topology: sustained run, candidates 0, migrations 0, valve untouched |
| `two_clients_alternating_on_one_directory_never_ping_pong_the_slot` | injected time, recording executor: cycles 1/2/3 issue, the 3rd ENGAGES the valve (rung-11 table shape), later attempts hold (migrations FLAT), past-cooldown re-promotes with evidence reset (no second demotion from one move) |
| `a_fenced_clients_placement_state_dies_with_its_incarnation` | wire-driven: a frame from a new `client_epoch` clears assignments + evidence (`fences` = 1) |

## No-regression legs (one run each, GREEN, this binary/venue)

* `s10-intents` — GREEN (mint/flush/visibility; OQ-2 live; storm priced;
  lever-off dark; MW-8 both sides; fsck+C8 clean ×2).
* `s10-delegation` — GREEN (engagement 26 hits/0 ships; coherence
  acked-before-publish; lever-off dark; kill-9 re-earn under the
  successor era; fsck+C8 clean).
* `s9-fanout` — GREEN (engagement exact, amp columns present, tripwires
  flat, fsck findings 0, drift 0).

## Gates run

* `cargo test --test mw_slot_placement_tests -- --test-threads=1` —
  **8/8** (red first at `622717f1`).
* Touched suites serial — all green: mw_intent_batch (20), mw_delegation
  (14), mw_recall_valve (12), meta_ship (15), env_knob_convention (21),
  derivation_sweep (37), metrics (9), skip_ledger (11+1 ignored),
  dynamic_meta_routing (16+1), meta_slot_migration (16), dlm_slot_lock
  (11), mw_ino_lane (9), mw_publish_era_gate (5), mw_cowriter_free (13),
  dlm_cowriter (18), meta_plane_distribution (4).
* `cargo clippy --all-targets --all-features -- -D warnings` AND the
  shipped config — clean. `cargo fmt --check` — clean.
  `shellcheck -x tests/run_mw_matrix.sh` — clean. Markdown check on the
  touched docs — clean.
* No new lock-free core (placement state is one `parking_lot::Mutex`
  over control-plane state — the rung-11/13 sanctioned class; the fast
  gates are relaxed atomics + an absent task-local), so no new loom
  model per the charter's preference.
* Full `task check` DEFERRED per the standing ruling for this ladder.
* Zero-residue teardown verified (0 `r14pl` nvmet subsystems, no
  daemons, no state/mount dirs, no stale `/dev/nvme*` plain files).

## Decisions (the ones a future reader needs)

1. **The hint lives at the ONE mint funnel, keyed on `SHIP_CLIENT`** —
   zero wire/schema change, covers shipped creates and both supply
   paths with one hook; the refill was moved INSIDE the client scope
   (it was outside — a silent rotor leak for refills).
2. **Client slots sit OUTSIDE the mint set** — the rotor never
   interleaves owner mints into a client's unit, so `migrate-meta-slot`
   moves the client's population, not a mixture.
3. **The policy triggers on supply events, candidate-first-after-
   threshold** — zero hot-path counting; the inversion is one arc-swap
   load + a volume scan, and an empty answer (every shipped fleet)
   leaves nothing but a saturated evidence counter.
4. **The valve reuses rung-11's constants and reasoning verbatim**
   (engaged-at-3, cooldown 8×, window = lease TTL) and has NO knob.
5. **The executor is the existing engine + the ownership re-arm** —
   no second migration plane (KD-MW-6's law: policy over existing
   machinery); installed only by the authority arm, so an unarmed
   process can observe but never move.
6. **The gate adjudication is the honest statement, not a torture of
   the venue** — the charter's own alternative; the structural reason
   (no client-ownable slot exists on a one-authority set) is stated in
   operations.md, rc-manifest, the knob text, and the stats comment.

## Residuals (stated, not hidden — the S11+ handoff)

1. **Per-volume claim admission (the fleet-of-authorities recipe)** is
   what unlocks the ≤1.10× gate: a client holding the D0 claim on ≥ 1
   volume of a shared set. Unbuilt — a format/guard rung of its own
   (the D0 Layer-B2 per-volume admission + partial-writer open + the
   claim set naming per-volume owners). The placement machinery is
   ready for it and pinned against its in-process shape.
2. **Cross-owner slot migration** — the engine writes both volumes
   through the local conveyor, so an authority cannot migrate a slot
   INTO a volume another node appends; the day partial claims land, the
   policy's vehicle needs a coordinated (shipped) migration form. The
   executor seam (`install_migration_executor`) is where it plugs in.
3. **Rung 13 residual 2 stands** (per-verb owner-side attribution of the
   ~11.7 S8 verbs/entry — a verb histogram on
   `meta_ship_owner_phase_ns`'s keying); this row re-confirms the
   magnitude (14.5 total verbs/entry with creates+utimes local) but does
   not decompose it further. Residual 3 (applied-name attr serves)
   stands with it.
4. **Supply-event economy observation**: 1,232 supply reservations for
   2,286 mints (~1.9 mints/event) — the per-directory grant carries its
   own supply, so wide shallow trees churn reservations. Each rides an
   existing reply (zero added wire), but a pooled-across-dirs supply
   would cut owner-side cursor/slot-gate work if a future row names it.
5. **The live gauge shape on the gate venue**: `client_slots` held flat
   at **2** across all four arms (one dedicated slot per metadata volume
   for the one co-writer identity — assignments survive the co-writer's
   remounts because they live on the OWNER, keyed by the client's stable
   KD-MW-2 id; 1,330 mints/arm hit the same two cached assignments).
   A persistent-slot census (which slots belong to which client,
   readable offline) is a fleet-of-authorities-era need, not today's.
