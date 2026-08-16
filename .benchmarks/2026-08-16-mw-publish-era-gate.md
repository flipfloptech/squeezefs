# 2026-08-16 — Finding #6 closed: the shipped-publish ERA GATE + the layout-publish idempotence witness (publish schema 5)

**Branch** `fix/mw-publish-era-gate` (worktree off dev `b68f933b`). The
rung-10b BLOCKER from the MW arming ladder: findings ledger **#6** + Named
residuals item 1 of `.benchmarks/2026-08-16-mw-s9-arm.md`. Adjudication
(design-first, the B4 pattern): **`docs/design-mw-layout-versions.md` §6a**
— written and committed before any code. Contracts:
`tests/mw_publish_era_gate_tests.rs` (5 pins, red-first). PR row 17
(`docs/design-full-multi-writer.md`) inherits the whole shape verbatim for
`WriteExtent`; PR row 10b (the default-format flip) was gated on this fix.

**Venue.** The `tests/mw_fleet.sh` tcp devsub (nvmet-tcp 127.0.0.1:54143,
`resv_enable=1`), fleet `create N=2 --membership --multi-writer
--cowriters=2 --lease-ttl-ms=6000`, authority bind `SQZ_MWFLEET_MW_PORT=
54193`. Binary: release, default features, `SQZ_BIN=$PWD/target/release/
squeezefs` on every rig invocation. Box: 32-core / 117 GiB, kernel
7.1.6-1-cachyos-sqz (co-located identities — the ops.md honest-residual
shape). Evidence tier: measured-simulated (one box; real mounts, real
nvmet-tcp wire, real PR). **No throughput claims here** — this is a
correctness campaign; the fan-out row's engagement gates are
load-independent.

---

## The adjudication (summary — the normative text is design §6a)

The s9-colocated-fence leg convicted TWO product gaps composing into a
durably-committed divergent layout-delta chain (fsck C1 on `TREE_XATTRS`,
220 C8 findings, `meta_kv_block_refs_drift = 48,620` — rows
`s9c-1786902746` in the rung-10 note). Bit 15 DETECTED it; detection is
not prevention. The three laws landed:

1. **The era gate covers the whole MUTATING publish vocabulary** (schema
   4 → 5): `SetLayoutAndSize` / `MergeLayoutAndSize` / `CommitBlockRefs` /
   `ParkWriteTimes` / `DestroyInodes` / `CreateWithRdevSize` carry
   `lease_epoch`; the owner refuses `PUBLISH_STALE_LEASE` when it is not
   LIVE custody (`validate_publish_era` — `check_free`'s law generalized;
   epoch-keyed for its three stated reasons), AFTER the authority check
   and BEFORE any window (a dead era's replay must never be answered from
   cache — the FreeBlocks precedent). A refusal applies NOTHING. Reads
   stay ungated (the S5 staleness class); raise/free/harvest keep their
   own landed gates and counters.
2. **The layout-publish class joins the RETRIED class under the
   `(lease_epoch, request_id)` witness** — stated against `publish.rs`'s
   no-retry doctrine, exactly the shape PR 17 schedules for `WriteExtent`.
   The owner serves the three layout verbs through S8's `DedupWindow`
   (never a third idempotence pattern; `meta_ship_publish.replays` is the
   engagement row). The client mints ONE id per logical publish
   (`next_ship_request_id`, the free/harvest sequence) and resends the
   SAME frame only (`ship_witnessed`: bounded ×3, epoch-stable,
   fence-class fast-exit — retries never re-key; an epoch move abandons).
   Past the budget the writeback ladder re-computes a NEW logical publish
   from current state, and the failed save **zeroes the RAM chain
   provenance** (`reset_layout_provenance`) so the recomputation claims 0
   and the §3 gate re-bases with a full `Put` — convergent, closing the
   lost-reply-then-transport-death refusal wedge.
3. **Ordering**: per-ino publish order is serialized end-to-end
   (`INODE_META_LOCKS` + the per-ino publish conveyor locally; the
   `PublishClient` lane mutex held ACROSS the round trip on the wire; the
   owner's own 4a + meta locks on apply), with the bit-15 version gate as
   the BELT — the divergent-base refusal is now pinned on the SHIPPED
   face (pin 4), not only the local one.

**The fence signal + the acked-un-fsynced law**: a stale refusal whose
presented epoch IS the client's current lease composes the FULL fence at
that round trip (`note_publish_era_refused`: UnknownLease machinery +
writer self-fence/poison — the pull-based revocation law), surfacing as
`WriterGuardFenced`; a refusal for a REPLACED epoch fences nothing
(`presented == current` is the guard). The zombie's acked-un-fsynced
writes are the POSIX crash class: with the one-way poison latch set,
constant-writeback fence-class units resolve as **verified fencing-stale
no-ops** (`writeback_fence_resolution` → `writeback_fence_noops`) instead
of retrying forever; staged bytes stay for the remount contract, and
fsync stays the loud error surface (no application is told discarded data
was durable). Operator text: `docs/operations.md` §Multi-writer co-writer
mounts.

New stats rows: `meta_ship_publish.stale_refusals` (0 healthy; growth
around a revocation is the gate composing), `meta_ship_publish.replays`
(the witness engaging), `writeback_fence_noops` (0 on every healthy
mount). No new env knob, no new incompat bit (the wire is a RAM protocol;
a schema-4 peer refuses loud at the first frame).

## The pins (red-first; `tests/mw_publish_era_gate_tests.rs`, 5/5 green; serial)

| # | Pin | Verdict |
|---|---|---|
| i | `a_stale_era_layout_publish_refuses_applies_nothing_and_is_the_fence_signal` — every mutating verb refuses in the fence class post-sweep, owner size + journal-entry count unchanged, `stale_refusals` +5, custody POISONED | green (red pre-fix: verbs applied) |
| ii | `a_duplicate_reship_answers_from_the_witness_and_never_clobbers_a_later_publish` — P1(claim 0→V1), P2(V1→V2), then P1 re-ships verbatim: answered from cache (reply equality), `replays` +1, **journal-entry equality**, P2's size survives (the divergence/clobber mint impossible) | green (red pre-fix: the duplicate re-based with P1's stale full layout) |
| iii | `a_fence_class_writeback_unit_resolves_as_a_verified_noop_only_under_poison` — poisoned+fence ⇒ resolve+count; live era ⇒ ladder; non-fence ⇒ ladder | green |
| iv | `out_of_order_and_cross_era_publishes_refuse` — divergent-base delta refuses across the wire naming §6.2 item 9 with nothing applied; a dead epoch refuses after re-join WITHOUT fencing the live era; the live era keeps landing | green |
| v | `the_solo_publish_path_is_structurally_untouched` — unarmed mount: local +5, shipped 0, `stale_refusals`/`replays`/`writeback_fence_noops` all flat | green |

Sibling suites re-run serial, all green: `dlm_multi_writer_tests` (16),
`dlm_cowriter_tests` (18), `mw_cowriter_free_tests` (12),
`mw_cowriter_lane_tests` (23), `mw_layout_version_tests` (11),
`mw_arm_s8_tests` (4), `meta_ship_tests` (15), `dlm_data_fence_tests`
(14), `mw_colocated_fence_tests` (1), `attr_publish_tests` (7),
`publish_phase_tests` (5), `mw_data_alloc_lane_tests` (23),
`mw_ino_lane_tests` (9), `dlm_membership_tests` (34),
`write_commit_economy_tests` (2), `publish_coalesce_tests` (6),
`publish_drain_economy_tests` (7), `write_commit_crash_tests` (2),
`durable_block_refs_tests` (14), `layout_delta_fold_tests` (9),
`fsync_writeback_tail_loss_tests` (3), `write_pipeline_tests` (22).
Fixture updates where the era gate is the point: the two shipping tests
(`the_daemon_publish_surface_ships_to_the_owner`,
`a_co_writer_ships_metadata_mutations_instead_of_committing_them`) now
hold REAL custody, and the free suite's stale-era pin asserts the fence
class + poison (the new composed contract) instead of message text.

## The standing repro: s9-colocated-fence ×3 GREEN FROM ZERO

Before/after on the leg's trailing oracle: pre-fix (rung-10 note, rows
`s9c-1786902746`) — **fsck C1 divergent layout-delta chain on
`TREE_XATTRS` + 220 C8 findings, drift 48,620**, the victim's queued
publishes APPLIED post-sweep. Post-fix, three full cycles, each
create-from-zero → leg → teardown-to-zero-residue (counted-restart
discipline: no fix landed mid-count, so the count stood):

| cycle | leg | fsck | drift | era-gate engagement (authority `stale_refusals`) | victim composition |
|---|---|---|---|---|---|
| 1 | GREEN (`rows/s9c-1786908655`) | `findings: 0 (clean)` | 0 | 4 (3× park_write_times + 1× set_layout_and_size refused BY ERA) | BY-ERA refusals in log → SELF-FENCED → POISONED; re-admitted by remount |
| 2 | GREEN | `findings: 0 (clean)` | 0 | 7 | **self-fence triggered BY the publish refusal itself** (`SELF-FENCED: S9: refusing a shipped publish …` — the new fence channel winning the race against the renewal loop, law 3 live) |
| 3 | GREEN | `findings: 0 (clean)` | 0 | 3 | renewal-loop self-fence first, publish refusals composing behind it |

Every cycle: victim fail-stopped client-side with zero device-rejection
lines (the S9-c class assertion), blast radius = the victim, victim
re-admitted by remount, `[mwfleet] teardown complete — zero residue`.
`replays` = 0 on the leg (no lost-reply retry fired in these windows —
the witness's deterministic engagement is pin ii; the leg proves the
GATE, which is what minted the old divergence). Row artifacts preserved
per cycle (fsck.out, m0 stats, victim log excerpts).

## The happy path stays green (one run each, fresh fleets)

* **s9-fanout GREEN** (`rows/s9a-1786909007`): 3 concurrent writers, W=4;
  W 403/410/423 MB/s, R 534/407/429 MB/s (provisional — correctness gates
  are the row's content); engagement exact (pub shipped 4907/4654,
  free shipped 205/195, lcr 0, enospc 0); amp 1.338× with the amp columns
  present and `block_free_*` flat; **fsck findings:0, drift=0** — the
  whole shipped stream now travels era-gated + witnessed with zero
  refusals on a healthy fleet.
* **s9-failover GREEN** (`rows/s9b-1786909093`): successor up in 1 s,
  fence scan 2/2 with zero silent survivors, both co-writers re-admitted
  under the successor era, **acked corpus byte-identical through the
  successor AND every re-admitted co-writer** (zero acked-data loss),
  re-admission stays the documented deferral.

## Gates

* Touched suites serial: all green (table above).
* `cargo clippy --all-targets --all-features -- -D warnings`: clean.
* `cargo clippy --all-targets -- -D warnings` (shipped config): clean.
* `cargo fmt --check`: clean.
* shellcheck: N/A (no shell script touched).
* markdown check: `design-mw-layout-versions.md` + `operations.md` PASS.
* Full `task check`: DEFERRED per the standing user ruling (not run).

## Residuals (stated)

1. **`WriteExtent` (PR 17)** inherits this shape by design-text (§6a's
   closing paragraph); it is not built here.
2. The witness window is RAM (S8's `DedupWindow`, FIFO-capped) — a
   duplicate arriving after an authority RESTART is not answered from
   cache; it is refused by ERA instead (the successor mints new epochs),
   which is the safe direction. The durable reply cache remains S3.5's
   named residual, unchanged.
3. `replays` engagement on the LIVE leg is 0 (no lost-reply fired in the
   freeze windows exercised); the deterministic witness proof is pin ii.
   A netem-shaped lost-reply leg is a cheap follow-on if a live row is
   ever wanted.
4. Throughput columns on the fan-out row are provisional (same-box cargo
   noise); the correctness/engagement gates are load-independent.
