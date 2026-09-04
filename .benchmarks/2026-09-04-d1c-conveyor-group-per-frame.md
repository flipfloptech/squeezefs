# 2026-09-04 — D-1c: one conveyor group per shipped frame (the D-1 ladder row's residual rung)

**Branch** `perf/d1c-conveyor-group-per-frame` (worktree off dev
`3066cfc3`). RED `37a93633` → fix `c2174469` → docs/rig (this note's
commit). Campaign: `docs/design-e2e-perf-audit.md` §5.3 **row 1**'s
residual rung, opened by the D-1 note and re-measured by the C-2 note
(`.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md` §Owed item 8);
contracts `tests/conveyor_tests.rs` (§"D-1c — group commit"),
`tests/publish_plane_batching_tests.rs` (§2b),
`tests/derivation_sweep_tests.rs`
(`a_shipped_frame_fits_one_conveyor_drain_at_every_width`), loom
`conveyor_group_never_drains_partially`. Target release: **1.2.1**.

## The problem (measured before this branch — not re-derived here)

D-1/D-1b made the owner dispatch a shipped frame's independent calls
concurrently so they *co-queue* on the M7 conveyor. Since C-2 put the
apply pass on the volume's own journal lane, the pass drains whatever
has arrived the instant it wakes — and a frame's N calls reach
`commit_tx` at N different instants, because every call's prelude awaits
(the custody check, the kvmap scoped-put probe, the inode read, the
custody-scoped compose). "One frame = one pass" therefore became
**arrival spread ÷ pass latency**: **3–14 owner passes per 24-publish
frame over 51 runs** (C-2 note item 8; the D-1 note's 4–8 passes per
64-verb frame is the same residual on the verb plane). Both D-1-family
contracts could pin the co-queue law only under a HELD pass
(`PassHold` / `TEST_CONVEYOR_HOLD_PRE_DRAIN`); the natural row was a
measurement.

## The mechanism (queue-side; the atomicity unit is untouched)

Three layers, each the smallest change that makes the law structural:

1. **`ConveyorCore::enqueue_many`** (`src/meta_backend/kv/conveyor_core.rs`)
   pushes a set of entries under ONE queue-lock acquisition. `drain` takes
   the same mutex, so a drain observes none of a group or all of it (FIFO,
   in the order given) — invariant #6 in the module doc, pinned by the loom
   model `conveyor_group_never_drains_partially` (a group committer's
   `enqueue_many` + `try_lead` against a retiring leader's
   release-then-recheck: exactly-once, never a prefix, no lost wakeup;
   `LOOM_MAX_PREEMPTIONS=3`, green with the six prior conveyor models).
   `enqueue` delegates to it.
2. **`KvMetaBackend::commit_tx_group`** (`backend.rs`): `commit_tx`'s
   pipeline over a `Vec<KvTx>` — every member is BUILT first
   (`build_queued_tx`: records, the per-volume value cap, the exact-size
   admission, the oneshot, the op-trace stamp), so a member that fails
   admission gets ITS `Err` and is never enqueued while its siblings are
   untouched; empty txs answer `Ok(())` inline; then ONE `enqueue_many`,
   ONE election, and every fan-out awaited in input order (the pass answers
   in journal order, which is the group's order). The parked-committer
   gauge and the named-wait census apply per member exactly as `commit_tx`
   applies them. `commit_tx` is the same pipeline over one tx
   (`conveyor_identity` / `lead_pass_if_elected` / `park_on_outcome`
   shared). `set_layout_and_size` is now `stage_layout_and_size` +
   `commit_tx`; `stage_layout_and_size_holding` is the group's form (the
   caller-held guard set); `KvTx` is a public OPAQUE handle.
   Gauges `META_CONVEYOR_GROUP_{COMMITS,TXS}` →
   `meta_conveyor_group_{commits,txs}` (txs ÷ commits = the live group
   size; 0 on a solo mount by construction).
3. **`RoutedMetaBackend::set_layout_and_size_group`** (`meta_backend/mod.rs`,
   `LayoutPublish { ino, layout, size, block_refs }`): the single verb's
   gates over the set (the S10 coherence permit and the §5.5.2a cutover
   pass, both held to the terminal outcome), `route_ino` after the gates,
   `check_volume_enabled` per item, the fail-stop mirror on error; grouped
   by home volume, each volume's members take **ONE canonical `lock_many`**
   over their inos, stage concurrently under the shared guard set, and
   commit through `commit_tx_group`. Outcomes in input order; an item that
   fails before the group (disabled volume, a stage error such as a missing
   inode) never joins it.

   **A law the task design did not state and the code needed**: the DLM
   `I{ino}` locks are STRIPED (4096 stripes), so two DISTINCT inos in one
   group may share a stripe. Per-member guards taken while siblings hold
   theirs would self-deadlock on a collision (`dlm.rs`'s module doc:
   "every operation that takes more than one metadata lock MUST come
   through `lock_many`"). The group's 4a acquisition is therefore ONE
   deduped, ascending `lock_many` — the canonical order every other
   multi-lock op already uses, so the group adds no new edge class to the
   wait-for graph. The same applies to the owner's serve stripes
   (`SERVE_INO_LOCKS`, 1024 stripes): the round's stripes are deduped and
   taken ascending before any prepare.

**Owner dispatch** (`src/meta_ship/publish.rs`): the `SetLayoutAndSize`
serve is split into **prepare** (the kvmap scoped-put probe — a reply here
is DONE and never groupable — the custody-scoped compose, the rung-19/20
refs recomposition, the over-cap ledger chunks as their own refs-only
txs) and **finish** (blob custody transfer, the displaced-blob and
recompute-released frees strictly after commit Ok, the reply; on `Err`
nothing is freed — the pre-split `?` shape). The multi-chain frame
proceeds in **rounds**: every chain contributes its next call; the round's
layout calls are screened (not-owner, era) exactly as serial calls, each
CLAIMS its `(lease_epoch, request_id)` witness slot synchronously
(`sqz_once::OnceCell::{claim_init, complete_init, abandon_init}` — a
manual-leadership face of the once-cell; a failed claim = a live winner or
a settled outcome, so that call takes the serial path, parks on the winner
and counts the replay as before), the claimed ones run ONE hop onto the
`sqz-meta` pool → serve stripes → concurrent prepare → one
`set_layout_and_size_group` → finish each → every witness completed with
its own outcome (an unwind is recorded and cached per member — a replay
answers the same loud failure). The round's other verbs serve concurrently
beside the group; a chain's next call starts only after its previous one
finished (chain order). A 24-distinct-ino frame is 24 chains of length 1
→ one round → one group → one pass. The single-chain path is unchanged.

**Lever** `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP` (registered, default on; `0` =
the D-1b per-chain dispatch verbatim — the same-binary A/B control, read
per served frame so a contract can flip it in-process). Engagement
`meta_ship_publish.frame_groups` (served frames that committed ≥ 1 group).

**Frame cap ≡ conveyor cap**: `router::batch_max_from` and
`resolve_commit_batch_txs` derive from the same root with the same shape
(`cpus × 2`, floor 64), so a full frame's group is never split by the tx
cap — pinned at nine widths; the byte cap may still split a group
(progress-first), pinned by `an_over_cap_group_splits_at_the_byte_cap_and_
commits_all`.

**Crash contract**: nothing about a group reaches the ring or the replay —
one tx = one checksummed journal entry. `a_group_replays_identically_to_
single_commits` commits 24 as one group with the checkpoint cadence
parked, drops without shutdown, reopens: 24 entries (= 24 single commits),
every layout and size rebuilt.

## The contracts (RED `37a93633` → green `c2174469`)

| Contract | File | Pins |
|---|---|---|
| `a_group_of_distinct_inos_is_one_pass_by_construction` | conveyor_tests | 16 txs → 1 pass (≤ 2 with one ambient), one `<=16` batch, 16 entries, every layout landed, every `I{ino}` free after |
| `an_inadmissible_member_fails_alone_and_its_siblings_commit` | conveyor_tests | one `ValueTooLarge` slot, 15 Ok in one pass, 15 entries, `group_txs` += 15 |
| `empty_groups_and_empty_members_commit_nothing` | conveyor_tests | empty Vec / inline `Ok(())`, no pass, no entry, no group counted |
| `two_racing_groups_commit_every_member_exactly_once` | conveyor_tests | 16 of 16, ≤ 2 passes (+1), 2 groups |
| `an_over_cap_group_splits_at_the_byte_cap_and_commits_all` | conveyor_tests | `BATCH_BYTES=64`: four batches of 1, all Ok, still ONE group enqueue |
| `a_group_replays_identically_to_single_commits` | conveyor_tests | entries(group) == entries(24 singles) == 24; replay read-back exact |
| `a_framed_burst_is_one_owner_conveyor_pass_by_construction` | publish_plane_batching | NO held pass: passes ≤ frames (+1), 1 ≤ groups ≤ frames, `group_txs + single frames == 24`, `frame_groups == groups`, entries 24, reply correlation |
| `the_group_lever_off_is_the_pre_rung_per_call_shape` | publish_plane_batching | `=0`: group gauges flat, row lands, entries 24 |
| `a_shipped_frame_fits_one_conveyor_drain_at_every_width` | derivation_sweep | `batch_max_from ≤ resolve_commit_batch_txs` at cpus ∈ {1…192} |
| `conveyor_group_never_drains_partially` | loom-models | `enqueue_many` atomic w.r.t. `drain`, exactly-once, no lost wakeup |
| `enqueue_many_is_fifo_and_counted` | conveyor_core unit | FIFO behind the queue, population answered, caps still govern |
| `manual_leader_parks_racers_and_completes_them` / `abandoned_manual_leader_lets_a_racer_reelect` | sqz_once unit | claim parks `get_or_init` racers (no racer init runs); abandon lets one re-elect |

Counted (debug, isolated target dir, quiet box): `publish_plane_batching_
tests` **20/20**, `conveyor_tests` **10/10**. Adjacent suites green:
`mw_publish_era_gate`, `meta_ship_owner_dispatch`, `mw_widthn_refs`,
`mw_layout_version`, `kvmap_mw_hazard`, `dlm_range_custody`,
`mw_cowriter_free`, `derivation_sweep`, `env_knob_convention`.

## Measured — in-process (release, default features, 32-CPU dev box, file-backed KV sandbox, loopback wire; two-node harness of the D-1b note)

The natural-row instrument (client drain hold 150 ms so the 24 provably
queue into one frame; NO owner pass hold), same binary, lever on vs off,
6 runs each. `wall` here is the client seam (the 150 ms hold), not the
mechanism — the D-1b measurement row (no seam) is the wall instrument.

| lever | frames | owner passes / frame | groups (txs) | `frame_groups` | entries | `tx_queue_wait` mean (n=24) | `pass_total` mean × n | Σ `pass_total` per frame |
|---|---|---|---|---|---|---|---|---|
| **on** ×6 | 1 | **1.00** (1/1, every run) | 1 (24) | 1 | 24 | 82–154 µs | 258–385 µs × 1 | **258–385 µs** |
| **off** ×6 | 1 | 2.00 (2/1, every run) | 0 | 0 | 24 | 106–151 µs | 152–243 µs × 2 | 304–486 µs |

Read: with the lever on, 24 publishes are ONE apply pass on every run —
the count is a property of the mechanism, not of the venue; the
serialized server's work per frame (Σ `pass_total`: one admission, one
union leaf-lock pass, one contiguous reservation, one submitted write)
drops ~15–25 % against two passes of 12. With the lever off, release
speed on this box collapses the C-2 note's debug-build spread of 3–14 to
2 — which is exactly the point: the un-grouped number is whatever the
box's arrival spread ÷ pass latency happens to be; the grouped number is 1.

The D-1b measurement row (no seams, derived depth) on the same binary now
reads **passes == frames** on every run (1/1, 1/1, 1/1, 3/3, 3/3, 3/4)
where the same row read 3–14 passes per 24 publishes on the C-2 branch.

## Field A-B-B-A — OWED (the parent runs it; root + tcp devsub)

Not run by the implementer (root + a devsub; the cloud rule does not
apply — this is the local fleet). The rig is written and syntax-checked:

```
sudo SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh create      # nvmet-tcp on 127.0.0.1 (MANDATORY: write row)
cargo build --release                                            # default features; BOTH legs = one binary
BIN=$PWD/target/release/squeezefs sudo -n -E env "PATH=$PATH" \
  bash .benchmarks/rigs/2026-09-04-d1c-fleet-lever-abba.sh        # L0a L1a L1b L0b: lever 0,1,1,0
```

Same shape as the C-2 lever rig (`2026-09-03-c2-fleet-lever-abba.sh`):
`tests/mw_fleet.sh create N=1 --multi-writer --cowriters=8` per leg, the
D-1b row (`2026-09-02-d1b-fleet-row.sh`, ROW_ONLY) — **8 co-writers × 24
concurrent `dd bs=1M count=128 conv=fsync` from `/dev/zero`** — torn down
to zero residue between legs; `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP` inherited
by the daemons. Analyzer `2026-09-04-d1c-fleet-analyze.py` (+ the C-2
analyzer for the journal-write hop split).

**Verdict columns** (authority m0, per leg, both brackets must agree):

| column | source | expectation |
|---|---|---|
| **passes per served frame** | `meta_conveyor_leader_passes` Δ ÷ `meta_ship_publish.served_frames` Δ | → **1.0** on L1a/L1b; the venue ratio (> 1) on L0a/L0b |
| group size / `frame_groups` | `meta_conveyor_group_txs ÷ group_commits`; `frame_groups` vs `served_frames` | ≈ calls/frame on L1; 0 on L0 (engagement) |
| `pass_total` mean + ρ(apply) | `meta_txpass_phase_ns.pass_total` | Σ per frame down; ρ(apply) down (fewer, fuller passes) |
| aggregate ingest GiB/s | the row rig's `aggregate` line | the headline — par or up; the D-1b note's ρ 0.97 authority is what this rung relieves |
| verbs/s per authority | (`meta_ship_publish.served` + `meta_ship.served_verbs`) ÷ wall | up or par |
| daemon CPU | `daemon_cpu_ns` Δ by class (`sqz-meta`, `sqz-jrnl`, `other` = incl. RPC lanes) | down or par (fewer passes = fewer admissions/leaf-lock passes/ring writes) |
| journal entries per publish | `meta_kv_journal_entries` Δ ÷ served | **unchanged** (one tx = one entry) |
| validity | ledger closure `served ≈ shipped`, `refusals` = `owner_panics` = 0, every stream rc 0 | the row rig exits nonzero otherwise |

Then the squeeze-test per the ladder row (§5.3 row 1's acceptance).

## Owed / next rungs

1. **The field A-B-B-A above** — landed-pending-field.
2. **The other publish verbs stay per-call by decision** (this rung's scope
   was the `SetLayoutAndSize` class): `MergeLayoutAndSize` (the Lever-B
   layout conveyor's own aggregation path — grouping it means composing
   with `publish_commit_group{s,_saves}`), `CommitBlockRefs`, `FreeBlocks`,
   `RaiseAllocLane`, the kvmap scoped-put train, and the S8 verb plane's
   `run_batch` (`src/meta_ship/service.rs` — the D-1 note's 4–8 passes per
   64-verb frame). Each is a candidate for the same prepare → group →
   finish split with its own contracts.
3. **Round lockstep**: within a frame a chain's next call waits for the
   whole round (the slowest prepare or serial verb of the round). On the
   field shape (the client serializes a file's publishes on its 3.5 stripe,
   so a frame carries ≤ 1 call per ino) rounds are 1 per frame; a frame
   with long same-ino chains pays R rounds. If the fleet row shows it,
   the lever is the control and the per-chain dispatch is one branch away.
4. **`task check` on this box** stopped at `tests/daemon_sigterm_exit_tests.rs`
   (2 failures: "daemon still alive 4 s after SIGTERM", "umount verb took
   6.3 s") — **reproduced identically at the dev tip `3066cfc3`** in an
   isolated worktree + target dir, i.e. pre-existing/environmental (a
   foreign `sigterm-probe` daemon from another session,
   `target/release/squeezefs mount … /home/justin/tmp/sigterm-probe/mnt`,
   was running on the box throughout). The remaining legs ran separately
   (see the final report).
