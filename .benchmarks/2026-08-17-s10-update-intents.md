# 2026-08-17 — Rung 13: S10 UPDATE INTENTS (KD-MW-13 — the serial-create recovery)

**Branch** `feat/s10-update-intents` (worktree off dev `cdacb7f7`).
Charter: `docs/design-full-multi-writer.md` §8.2 lever 1 (the UPDATE arm)
+ PR row 13 — *"meta_ship intent batches, per-directory EXCLUSIVE UPDATE
grants + recall-on-conflict, dentry-version revalidation at grant,
deferred-refusal latch (`meta_ship_intent_refusals`), fsync(dir) flush
force, MW-8 crash repro; recall-forces-flush (OQ-2's resolved form)
priced by the storm row"* — consuming the rung-11 valve API and the
rung-12 delegation machinery verbatim, and discharging rung-12 residuals
1 (UPDATE intents — the measured tar-x motivation) and the ENTRY-face of
its TTL residual (the census makes per-name locality real).

**Red-first discipline.** The whole suite
(`tests/mw_intent_batch_tests.rs`, 20 arms) landed first and fails to
compile at its commit (`9466ae94`). The OQ-2 red HALF is permanent:
`red_half_without_the_read_gate_a_foreign_lookup_misses_acked_names`
runs the exact foreign-read shape against the gate-off seam
(`TEST_INTENT_READ_GATE = false` — a test static, never a knob) and
proves the stale foreign NEGATIVE happens — the gate IS the mechanism.

---

## What landed

* **Wire (schema 2 → 3**; KD-MW-11: no incompat bit — intents are a RAM
  protocol): per-op results carry the optional **`IntentGrant`** (the
  EXCLUSIVE UPDATE authority riding a shipped create's reply — the
  intent-lock law applied to mint authority; target preference: the
  created DIRECTORY on a mkdir, else the parent), and the new
  **`VERB_DELEG_INTENT`** (0x0402) carries ordered batches of
  `CreateAt` (pre-supplied global ino + the client's mint instant as the
  record times) and `SetattrAt` (the deferred tar-utime shape;
  deliberately sizeless — size changes are data-plane acts and barrier +
  ship). `STATUS_INTENT_LEASE` (39) is the custody-fence refusal.

* **The census IS the dentry-version revalidation** (the §8.2 law-1
  requirement, discharged as the entry SET): the grant carries D's
  complete name census (budget = `CONTROL_MAX_FRAME_BYTES/2/64`,
  clamp 256..65536; an over-budget directory DECLINES — the design's own
  priced fallback). A volume-wide watermark deliberately does NOT gate
  mints: rung 12 measured that a commit-frontier stamp never settles
  under a create storm (its tar-x wash), which would structurally
  disengage the exact workload this rung exists for. Exclusivity keeps
  the census exact forever after (every foreign mutation of D recalls
  the grant first; the holder folds its own applies in), so a local
  negative is authoritative and **O_EXCL is decidable locally with NO
  warming window** — plus the census answers authoritative local
  NEGATIVE lookups (the tar `open(O_CREAT|O_EXCL)` kernel-LOOKUP leg,
  zero wire).

* **The ino supply is a grant-carried OWNER CURSOR RESERVATION**
  (`reserve_intent_supply`: one (volume, slot) cursor advanced by the
  derived chunk `clamp(batch_max × 4, 256, 65536)`; the global encoding's
  affinity over the range is asserted, never assumed). **Stated
  deviation-in-mechanism from §8.2's "mints from its own lane (bit 12)"
  wording, conformant in law**: a literal bit-12 residue lane requires
  volume-wide PARTITIONED MINTING (every minter laned, the owner's own
  native cursor included) — a format-engagement rung of its own, and
  without it a co-writer lane collides with the owner's dense mints.
  The reservation is collision-free within the owner incarnation by the
  cursor advance, and across incarnations by the ERA GATE: the flush
  frame presents the SUPPLY's own `owner_term`, so a successor refuses a
  dead era's numbers whole before its recovered cursor could collide.
  Unused inos burn free (§4.8's own law, quoted by KD-MW-13 itself).
  Placement is single-slot per supply — mint-spread/client-owned-slot
  placement for intent mints is EXPLICITLY row 14's lever.

* **The apply**: `create_with_rdev_preset` — the production create body
  with an explicit (local, global) pair + `ts_override` threaded through
  `routed_create_local`/`routed_mint_inode`, and the **idempotent-replay
  tiebreak** (an existing dentry naming exactly the preset ino is the
  op's own earlier apply — the witness-by-construction the explicit ino
  buys across owner failover). Era-gated (owner term exact-match + the
  custody `lease_epoch` via `validate_publish_era` — the publish path's
  own validator; **intents die with the custody fence**, refused BEFORE
  the witness window) and witnessed per op on `(lease_epoch,
  request_id)` through the S8 `DedupWindow` (the rung-9 finding-#6 law
  FROM BIRTH — never repeated). Applies run under `SHIP_CLIENT` +
  **`SHIP_INTENT_APPLY`** scopes: the mutation gate keeps the flushing
  holder's own UPDATE grant alive (surrendering it per op would orphan
  the owner-side exclusivity record and recall the flusher into its own
  flush — the deadlock the exclusion exists for), recalls every FOREIGN
  holder via the lane's new `recall_object_excluding`/`holders_excluding`
  arms, and rides the holder's LOOKUP-grant surrenders back on
  `IntentResult.revokes` (the self-conflict law, this verb's face).

* **recall-forces-flush (OQ-2's resolved form)**: `revoke_delegations`
  flushes a recalled UPDATE authority's batch BEFORE the drain/ack — the
  O_EXCL exactly-one-ack ORDERING proof (a foreign create's mutation
  gate cannot proceed until the holder's acked names applied) — and the
  **OQ-2 read gate** recalls the grant on any foreign lookup/readdir
  under D, on ALL THREE faces: shipped verbs (`execute`), the owner's
  own trait lookup/readdir, and the FUSE `readdir_stream` pager (live
  finding #3 — the handler rides the non-trait pager, which was a
  coherence hole).

* **The deferred-error law** (§8.2 law 2, POSIX-16 errseq precedent): an
  apply refusal latches onto the DIRECTORY, surfaces ONCE at
  `fsync(dir)` (the FUSE fsyncdir hook; `fsync(file)` on a pending mint
  flushes first and reports a destroyed mint's poison), destroys the
  local mint (images/names dropped, kernel inval via the delegation
  sink, ops on the destroyed child answer the owner's errno), and counts
  `meta_ship_intent_refusals` (must-stay-≈0).

* **Ordering barriers**: every SHIPPED verb (S8 trait + S9 publish
  surface) naming pending state — a pending ino, a pending (dir, name),
  a dir with queued ops — flushes first, so owner-side application order
  always respects local causality (a publish or unlink can never
  overtake the create it names). One relaxed load when nothing pends.

* **Flush triggers**: `fsync(dir)`/`fsync(file)` synchronous (the
  contract point); recall (before the ack); the ordering barrier; the
  size bound (`batch_max`); and the **delayed, coalescing RELEASE kick**
  (one negative-TTL window — the same term the published visibility
  bound derives from, one law; live finding #2: an immediate per-release
  flush raced tar's post-close `utimensat` — every explicit time-set
  arrived at an already-applied ino and SHIPPED — and collapsed batching
  to one frame per file). Transport-dead flushes re-queue (order
  preserved; fsync reports EIO, retryable); terminal era/custody/deleg
  fences latch + destroy the batch (the §8.2 error channel) and drop the
  lane's authorities.

* **`SQUEEZEFS_UPDATE_INTENTS`** (ENG-10 registry; `Kind::Bool`, static
  default on, read only when armed AND under `SQUEEZEFS_DELEGATION` —
  the lever form verbatim; `=0` is the tar-x row's control).

* **Stats**: the `meta_ship_intent` object — the design-§13 spellings
  (`batches`/`verbs`/`flush_forces`/`refusals`) + engagement gauges
  (`mints`, `declines`, `local_eexist`, `local_negatives`,
  `deferred_setattrs`, `mint_destroys`, owner-face
  `applied`/`replays`/`stale_refusals`/`read_recalls`/
  `update_grants`/`update_declines`, gauges `pending`/`authorities`/
  `supply_remaining`) and the published §8.2 bound
  **`meta_ship_intent_visibility_bound_ms`** (post-`fsync(dir)` the
  batch-flush term is 0; the published number is the foreign-kernel
  negative-TTL term — foreign kernels' TTLs are ≤ the reader bound by
  the S5 TTL law, and reader mounts add their own published
  `reader_staleness_bound_ms`).

## The live findings (each caught by the fleet, invisible to cargo — the rung-12 lesson repeating)

| # | Finding | How it presented | Fix |
|---|---|---|---|
| 1 | **The D2.c parent-attr refresh shipped one getattr per minted create** — the FUSE create handler refreshes parent attrs after every create; the parent had no servable image | the leg's mint span: 24 creates = 96 → 24 shipped verbs after the first two fixes, attributed by per-op live probes | the IntentGrant carries D's FULL attr image; the holder FOLDS its own mints in (Δtimes, mkdir Δnlink — the same Δ the apply stamps) and the router's getattr serves it while the authority is live. After: **24 creates + 24 deferred utimes = 0 shipped metadata verbs** |
| 2 | **The immediate release-kick defeated deferral AND batching** — tar's post-close `utimensat` found an already-applied ino (2 wire verbs/file), one frame per file | the same mint-span probes | the kick is now DELAYED one negative-TTL window and coalescing (one scheduled kick per lane); foreign reads stay exact regardless (OQ-2), the delay only bounds the MW-8 window. Live coalesce factor: 25 intents/frame |
| 3 | **`readdir_stream` was an OQ-2 hole** — the FUSE readdir handler rides the non-trait pager, not the trait readdir; the owner's own `ls` could serve a page missing un-flushed foreign intents, unboundedly | the storm row's `read_recalls` stayed 0 while the owner read the hot dir | the pager now runs the same local read gate |
| 4 | **The delegation fence keyed the identity STRING** — "re-admission is by remount" was a dead letter when the HOLDER died: the remounted successor presents the same KD-MW-2 id and stayed fenced forever, un-grantable | the MW-8 kill's remounted co-writer earned no authority | the fence records the INCARNATION (`client_epoch`); any frame from a fresh incarnation clears it (`note_client_incarnation` at every frame admission); the zombie's own epoch stays refused |

Plus the instrument lesson (finding #0): owner-face counters live on the
OWNER's stats inode — the leg's first failure was reading
`update_grants` through the co-writer; the client-face absorption gauge
(`meta_ship_intent_authorities`) landed for exactly this.

## The live leg — `tests/run_mw_matrix.sh s10-intents` (GREEN from zero on the final binary)

Venue: `mw_fleet.sh create N=2 --multi-writer --cowriters=1`
(instance-suffixed tcp devsub, nvmet-tcp 127.0.0.1:54143;
`SQZ_MWFLEET_MW_PORT=54193`; `SQZ_MWFLEET_OSS_GB=24`), release binary at
`811aff28`, kernel 7.1.6-1-cachyos-sqz. Counted-restart held: every fix
above tore the fleet down and re-ran from zero.

| Phase | Verdict |
|---|---|
| **Mint engagement** | 24 creates + 24 deferred utimes under the earned grant = **24 mints / 0 shipped metadata verbs**; `fsync(dir)` drained the span (pending 0) at **coalesce 25.0 intents/frame**; the authority sees every name + the deferred times EXACT (`mtime == 1700000000`) |
| **OQ-2, live** | the owner's own read under the granted dir RECALLED the holder (read_recalls +1), the recall FORCED the flush (flush_forces +1), and the acked-un-fsynced name `unsynced` served — no fsync ever ran for it |
| **The STORM row (OQ-2's price)** | 12 rounds of {co-writer create+write, owner `ls`} on one hot dir: **1.8 ms/round wall**, 1 read-recall, 11 forced flushes (the write-publish barriers), 0 valve demotions, 0 recall timeouts, 0 stale serves. **OQ-2 does NOT reopen**: the foreign-read price is one recall+flush round (~2 ms at rtt≈0; ≤ recall deadline at any RTT, bounded by the rung-11 arithmetic), and the cargo storm (`the_grant_ping_pong_storm_engages_the_valve_before_fanout_hurts`) pins the valve engaging before fan-out hurts on the true ping-pong shape |
| **Lever-off control** | `SQUEEZEFS_UPDATE_INTENTS=0` remount: 0 mints, 0 grants, every create shipped — the A/B is dark |
| **MW-8, both sides** | pre-fsync kill-9: 16/16 applied THIS run (the delayed kick's window had elapsed — the disclosed class permits any FIFO PREFIX, and the prefix law is the gate; the deterministic-loss pin is the cargo arm); post-fsync kill-9: **16/16 durable** (fsync(dir) IS the contract point). fsck findings 0 + `meta_kv_block_refs_drift` 0 after EACH kill; the incarnation fence re-admitted the holder both times |
| **Tripwires** | `owner_panics` 0, `publish.refusals` 0, `invariant_tripwires` 0, `stale_serves` 0, `meta_ship_intent_refusals` 0, `local_commit_refusals` 0 |

## The measured tar-x row — `tests/run_mw_matrix.sh s10-intents-tarx`

The rung-9 instrument VERBATIM: real linux-src `fs/` tree (2,384
entries, 47 MB), netns co-writer, netem 125 µs/end = **250 µs wire RTT**,
A-B-B-A (on, off, off, on), quiet-gated. Engagement exact: ON arms
minted 2,286 (the 98 remainder = the earn/first-in-dir ships), OFF arms
0.

| Arm | wall s | entries/s | mints | S8 ships | publish ships | wire verbs/entry |
|---|---|---|---|---|---|---|
| int-on-1 | 14.70 | **162** | 2,286 | 28,072 | 6,205 | **14.38** |
| int-off-1 | 16.57 | 144 | 0 | 35,070 | 6,309 | 17.36 |
| int-off-2 | 16.45 | 145 | 0 | 35,012 | 6,414 | 17.38 |
| int-on-2 | 14.78 | **161** | 2,286 | 28,080 | 6,192 | **14.38** |

Honest reading, order-independent (A-B-B-A agrees to <1 %):

* The OFF arms REPRODUCE rung 9's published 143/s at 250 µs (144–145/s —
  instrument parity, cross-checked against
  `.benchmarks/2026-08-17-s10-delegation.md`'s table).
* Intents ON: **+12 % entries/s (161–162 vs 144–145), −17 % wire
  verbs/entry (14.38 vs 17.36)** — real, engaged (creates + utimes off
  the wire: 2,286 mints, ~2,286 deferred setattrs), and NOT yet the
  ≤1.10×-of-local recovery (local S0 ≈ 1,200–1,300/s). The residual is
  named, not hidden: **~11.8 S8 verbs/entry still ship on the ON arms
  with the create/utime plane fully local** — the data-plane write path
  (per-file lease/custody ceremony + the inline-publish stream) and the
  kernel's remaining per-entry metadata reads (census-positive lookups
  ship BY DESIGN in this rung — no attr image for arbitrary children),
  which is exactly the decomposition row 14 (client-owned-slot
  placement) takes as input. Row 14 owns the formal gate; this row is
  its input, published either way per the charter.

## FOUND (pre-existing, BLOCKING the tarx leg's oracle): C8 drift under the co-writer tar+rm shape

The new `s10-intents-tarx` leg is the FIRST to run the fsck/C8 oracle
after a co-writer tar-extract + `rm -rf` sweep (rung 9's `s8-serial-ab`
never fsck'd its venues), and it found **`[C8] durable block-reference
drift: 1 durable record vs 0 counted layout references`** (11 findings
after the 4-arm sweep). **Attribution: NOT this rung's machinery** — the
counted control (fresh fleet, `SQUEEZEFS_UPDATE_INTENTS=0`, 0 mints, one
tar+rm pass, every rung-13 path structurally dark behind its zero-gauges)
reproduces **5 C8 findings**. The leak is the pre-existing S9
shipped-publish/shipped-free composition on data-bearing co-writer
workloads (durable refs staged by shipped layout publishes surviving the
displaced/terminal frees of overwrite-during-extract + `rm -rf`). The
leg KEEPS its oracle gate (it should fail until the leak is fixed) and
exits nonzero on the fsck step after publishing the table; the fix is a
ladder rung of its own (repro: one tar+rm pass on any co-writer fleet,
intents irrelevant). **This is the rung's stop-and-read handoff.**

## Gates run

* `cargo test --test mw_intent_batch_tests -- --test-threads=1` —
  **20/20** (red first: `9466ae94` fails to compile by construction; the
  OQ-2 red half is permanent).
* Touched suites serial: mw_delegation (14), mw_recall_valve (12),
  meta_ship (15), env_knob_convention (21), derivation_sweep (37),
  metrics (9), skip_ledger (11+1 ignored), mw_publish_era_gate (5),
  mw_cowriter_free (12), dlm_cowriter (18) — all green. (Two mechanical
  pin updates: the schema-2 pins in meta_ship/mw_delegation now name
  schema 3; two `MetaOpResult` fixture initializers gained
  `intent_grant: None`.)
* `cargo clippy --all-targets --all-features -- -D warnings` AND the
  shipped config — clean. `cargo fmt --check` — clean.
  `shellcheck -x tests/run_mw_matrix.sh` — clean. Markdown check on this
  note — clean.
* No new lock-free core (the intent lane is one `parking_lot::Mutex`
  over control-plane state — the rung-11 lane's sanctioned class; the
  fast gates are relaxed atomics), so no new loom model per the
  charter's preference.
* Full `task check` DEFERRED per the standing ruling for this ladder.
* Zero-residue teardown verified (fleet state gone, 0 mwfleet nvmet
  subsystems, 0 mwfleet null_blk disks; the mid-campaign SIGKILL'd
  substrate was manually reaped and the reap verified).

## Decisions (the ones a future reader needs)

1. **Census-carried grants, not watermark-gated mints** (above) — the
   §8.2 revalidation requirement discharged as the entry set itself;
   rung 12's measured disengagement is the reason.
2. **Supply = owner cursor reservation, era-fenced** (above) — the
   stated mechanism deviation from the bit-12 wording; the collision
   laws hold and volume-wide partitioned minting is left to its own
   rung.
3. **UPDATE grants ride ONLY shipped-create replies** (parent +
   created-dir), one per reply; the publish path's `CreateWithRdevSize`
   (symlinks) can MINT under an existing grant but never earns one —
   its reply vocabulary carries no grants (stated, not hidden).
4. **The intent apply keeps the flusher's UPDATE grant**
   (`SHIP_INTENT_APPLY` + the lane's excluding arms); every other
   mutation keeps rung 12's surrender law verbatim — an UPDATE holder's
   own shipped mutation in D (unlink/rename) surrenders and re-earns,
   priced, after the pre-ship barrier flushed.
5. **Terminal-fence flushes destroy; transport-dead flushes re-queue** —
   the §8.2 split between the error channel (era/custody/deleg fences:
   latch + destroy, never silently absorbed) and the never-lossy posture
   for transients (order-preserving retry; fsync answers EIO).
6. **The tarx leg's oracle gate stays** despite the pre-existing C8 leak
   it exposed (above) — a leg that averts its eyes from a red oracle is
   the anti-pattern; the row's TABLE is valid (timing + engagement), the
   EXIT is honest.

## Residuals for row 14+ (stated, not hidden)

1. **The C8 drift leak** (above) — pre-existing, now oracle-visible,
   blocking the tarx leg's green exit. Fix rung before row 14's gate can
   run its instrument end-to-end.
2. **The ON-arm residual ~11.8 S8 verbs/entry** needs per-verb owner-side
   attribution (a verb histogram on `meta_ship_owner_phase_ns`'s keying)
   — the row-14 placement lever's input. Candidates measured live:
   census-positive lookups (ship by design — no child-attr image),
   per-file write-path ceremony, release/flush-class verbs.
3. **Applied-name attr serves**: census-positive names of APPLIED
   children ship their lookups/getattrs; a bounded name→ino(+attr) cache
   under the same exclusivity argument would delete more of residual 2 —
   priced only if row 14's decomposition names it dominant.
4. **Partial acks** (rung-11 residual 3) — still unpriced; the storm
   rows never queued deep enough.
5. **Supply placement is single-slot** — mint-spread for intent mints is
   row 14's (client-owned-slot placement), stated at the reservation.
6. **`SQZ_DEVSUB` NQN-reuse reconnect latency**: repeated same-instance
   fleet create/teardown cycles eventually push the initiator's
   namespace surfacing past dev_substrate's 10 s wait (observed
   post-SIGKILL; a fresh instance name connects instantly) — a rig
   nuisance, not a product bug; noted for the fleet-rig owner.
