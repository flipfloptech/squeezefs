# Rung 6 wave C2 — membership shards, the death ledger, dead-appender recovery, WERO fencing with registrants, park-and-reclaim, the join ladder over the wire

Spec: `docs/porting/sym_wire_membership_recovery.md` §2–§9; charter §5.5 ("Membership is sharded by
home volume", "A manager's death is its home shard's outage", "Death and recovery (§5.9)", "Fencing",
"Flush ceiling"), §5.7, §8 (`appenders`, `appender clear`). Decision 0002 rows 264–271; 0003 §E
(member rows as `WriterClaimV1.kind = 1`). Modules: `src/meta/sym/membership.zig` +
`src/meta/sym/membership/{shard, death_ledger, recovery, park_reclaim, park_core_model, fencing,
join_ladder}.zig`; `src/cli/appenders.zig`; hunks in `meta/sym/control_records.zig` (`Tag.member`,
`memberKey`), `meta/guard/claim.zig` (`KIND_MEMBER`), `meta/guard/fake_nvme.zig` (the capped
array), `meta/sym/slots/lease_table.zig` (the abort keeps the dead mark), `cli/{args, cli}.zig`,
`core/knobs.zig` (one `SQUEEZEFS_TEST_*` row), `stats/stats.zig`, `meta/sym.zig`, `squeezefs.zig`.

## What landed

- **Membership shards** (`shard.zig`): the owner's RAM authority per home volume — `arm(term,
  prior)` refuses a stale term; `join` (reclaim = `prior_epoch ∨ expected`; the two-halves grace
  window: the fresh-refusal half closes early once every expected member re-asserted, the
  re-assertion half admits a prior epoch to the deadline; `UnknownLease` for a reclaim of an epoch
  the owner does not hold once that half closed; the OQ-5 mount-slot collision naming the other
  mount point); `renew` = one probe + two stores, commits nothing; `expireDue` evicts at `≥ T_owner`
  and ships `DeadMember{epoch, pr_key}` to the death sink; `reapGraceDeadline` ships the
  non-reclaimers with `epoch 0` + the roster's key, once; three-valued `liveness`; the departed
  keys kept `2 × T_owner`; the census page; the renewal CARRIAGE (`pushOffer` / `pushReleaseNotice`
  → the member's next `Grant`). Member rows: `MemberRow` = the writer claim's 88 B shape at
  `kind = 1` under `member:{node ‖ slot}` on the home volume's tree 0, written ONCE per change
  (`membership_registration_commits`), the `clients` seam through the ONE claim classifier (the
  dead-pid proof applies). `OwnerService` serves `join/renew/leave/census` on the ONE listener;
  `Member.tick` is the renewal tick decided: idle between beats, renewed at the cadence, a dead
  venue past `T_self` PARKS a symmetric appender (never the poison), `UnknownLease` before `T_self`
  fences terminally, the reclaim beat rejoins once the successor admits it. `LeaseClocks.derive`
  (skew/purge/T_self/renew/grace) from `T_owner` alone; `tOwnerMsFromEnv` reads the one seam.
- **The death ledger** (`death_ledger.zig`): the writer over the data plane's ONE record authority
  (`alloc_lease.recordDeath` etc.) — a reader's id records nothing, a refused write parks in RAM
  (dedup by identity, `dead_members_deferred`) and the next poll drains it; `retireForJoin`;
  `screenDeath` (own identity / a live member / a held or contradicting key rejected before any
  write; an unknown census accepted and COUNTED `dead_member_keys_unvalidated`); the vol-0 rule
  (`noteVol0Probe`: unreachable > `T_owner` releases the role, once); `Projection.run` = the poll
  (release-recovered first, every death on every volume with `dead_members_acted` per region and
  `dead_member_propagation_ms` as the max lag, the retirement at `2 × T_owner` only when no
  Live/Recovering page stands and no lease names the identity, the roll-forward iff anything was
  recovered); `LedgerHandlers` serve `RecordDeath` / `RecordRecovered` (`not_manager` off volume 0;
  `already` on the replay); `recordDeathOver` / `recordRecoveredOver` are the peer's clients.
- **Recovery** (`recovery.zig`): the §5.9 procedure as ONE `Driver.recoverRegion` over a `Volume`
  seam — steps 0 (the fresh re-read, the standing record, a rejoined member retires it) → 1 (the
  preempt with the own-key guard; `pr_fenced ≡ landed`) → 2 (`Recovering{writer_term}` + the rule-4
  suspension) → 3 (the window ≥ the tail, tree 0's leases + grant, the three violation tripwires,
  ABSORBED slots on a re-run) → 4 (`Releasing{dead}` in the table, the root by `g` never by seq
  installed after the cache barrier, cursor/extents/`seq_floor` floors) → 5 (interior `(level DESC,
  seq)` then content, the data deltas) → 6 (cycles until clean) → 6b (every leaf's tail on non-PR,
  the resident ones on PR) → 7 (ONE entry per slot: `Unleased` + `slot_tails` + the quarantine iff
  EARLY; the grant minus the trees' images) → 7b (the table LAST, the floor raised, the rollback
  disarmed) → 7c (the unclaimed grant + the orphans returned) → 8 (`Recovered{ledger_tail_seq =
  head}`, slots cleared) → 9 (`recovered:`). The RAII `Rollback` (§3.1) on any error between 4 and
  7b: `Releasing{dead} → Leased{dead}`, the tree restored to its pre-install words or removed when
  adopted fresh, the doors woken. A `Recovered` page without its record is completed by the poll.
  `recoveryBoundMs` (1,105 / 1,128 / 1,312 ms at 512 KiB / 4 MiB / 32 MiB). `clearAppender`: the
  seven refusals in order, the four writes in order, idempotent after a crash past write 1.
- **Park-and-reclaim** (`park_reclaim.zig`, `park_core_model.zig`): the gate's word `RUNNING →
  PARKED → RUNNING | EXPIRED`; `tryEnter`/`park` the Dekker pair (SeqCst both sides); the
  pre-admission gate (register-recheck-await, never the escalation); `holdAck`; `release` /
  `expireIfDue` / `expireNow` as ONE CAS each; `leaveAllowed` latches the set-wide refusal;
  `fenceAtTSelf` splits by class; `reclaimUnderPark`; `T_park_max = failover_bound + grace`
  (91.5 s at the design's row). The model holds and both weakenings (either fence removed; a
  load-then-store exit) fail.
- **Fencing** (`fencing.zig`): `acquireWero` (rtype 3), `joinAsRegistrant` (ONE key, the report
  sized by REGCTL, the cap gate — declared else learned else unbounded, learned ONLY on the repeat
  of a device-answered command-specific status at the same regctl, unlearned on a later success;
  a refused registrant leaves no residue), `adoptColocated` (zero device mutation; the holder
  cross-checked against the enrolled keys; rtype 1 refuses), `preemptDeadRegistrant` (fenced /
  own_key_guard / no_key / no_device / failed), `WriterTerm` (the era + 1 durable before the open
  proceeds, never lowered; tokens `(term << 40) | seq`), `ScreenSource` (the slot's `(g, lessee)`
  with the Recovering SUSPENSION nulling the lessee while rules 1–3 stay armed), `FrameStamps`
  (the priming law; `appender_unstamped_frame` fires on a stamp asked of an own leased slot before
  the prime; a re-adopted lease stamps `g`, a native slot `g + 1`).
- **The join ladder over the wire** (`join_ladder.zig`): rungs 1–3 as pure decisions (`MW_BIND=off`
  refuses); `ManagerHandlers` — the manager's wire face for the eight region verbs and the five slot
  verbs (through the plane's `HandlerTable`, the reply decoded back into the typed shape) with
  every answer the ONE status table's word (capacity → `busy`, never the service's must-stay-0);
  `ManagerWire` — the joiner's session as the `join.Door` with the `redial` flag and the
  `joined_wire_*` gauges, the page's words read at `page_addr`, the grant from the reply's word;
  `Joiner` — rungs 5–7 (join, `M` rotor slots, first touch, `PublishRoots`, the carriage's accept
  of a pushed OFFER = the handover over a dial-only wire); `SameNodeProof` (the D0 flock proves dead
  exactly the predecessor manager page 0 named at the open); `FlushCeilingAudit` (holds by class,
  the recovery class capped at the bound, the service class at one ceiling, the 64-cycle tape).
- **CLI** (`cli/appenders.zig`, Linux): `squeezefs appenders [--json] <uri>` lists every page off
  the device without the flock; `squeezefs appender clear <uri> <id>` runs the seven refusals over
  the offline probes (the flock, the claim through the gate's probe, the page, this node's token,
  the parked-joiner window, `gate.clients`) and the four writes (the page `Recovering{0}` raw to
  both slots + fsync; the set opened through the writer door; the EARLY record on volume 0's
  control tree committed + barriered + checkpointed; the close).

## Measured / counted

- Portable (macOS): 1,505 tests pass; the two-daemon LOOPBACK contract — B joins A over the hub,
  acquires 2 rotor slots, first-touches slot 7, slot 1 (A's) answers `SlotBusy{holder 0}`,
  `PublishRoots` lands in tree 0, the endpoint round-trips, A's `OfferSlot(2 → B)` rides B's renewal
  `Grant` and B's accept over the wire runs A's transfer (`slot_handovers` +1), every lease released
  over the wire, the clean leave `already` on the replay.
- Linux (the venue): the REAL-TCP leg — B dials A's `wire/listener`, joins, acquires, touches, is a
  member; A's listener stops; B's beats fail quietly, at `T_self` B PARKS (`appender_parks` +1, the
  poison never called); the successor arms at term 2 expecting B on a NEW port; B re-dials through
  the seam and the reclaim lands in grace (`slot_lease_reclaims` +1, liveness `live`).
- Recovery over RAM images: every one of the 13 crash windows (a failure right after each step's
  durable effects) re-runs to the converged state — tree 0 `Unleased` per slot at the same `g`,
  cursor 72 = `max(40, 50, 71 + 1)`, `seq_floor` 4990 = the dead frontier, the page's NEWER root
  (`0x10000`) installed over the manager's grant-time root, the window flushed in order `(3, 1, 4)`
  (interior first), `Recovered{ledger_tail_seq = 5000}`, `recovered:` present; the rollback after
  steps 4–7 leaves `leased{dead}` (a first touch refused), the stale tree restored and the fresh
  guest removed; a re-run past tree 0 ABSORBS both slots; the full tail scan reads 3 × 64 KiB on
  the file substrate; an EARLY record quarantines for `(1,000 + 45,000 + 44) ms`.
- Fencing with the fake controller: WERO puts rtype 3 on the wire; a declared cap 2 refuses the
  third registrant loud and leaves the table at 2; the emulated capped array's `0x180` is a
  candidate on the first refusal and the LEARNED cap on the repeat; the array's growth unlearns it;
  the usurper's key is preempted-and-aborted and its writes refused at the device.

## Not in this wave (stated)

- The daemon's binding: `mount` does not yet start the ONE listener nor arm the shard owner, the
  ledger poll, the park gate or the recovery driver over the real backend — the two-`squeezefs`
  daemon kill matrix (kill -9 B ⇒ eviction ⇒ recovery ⇒ remount as a successor; kill -9 A ⇒ B parks)
  and the ≤ 1 GiB smoke stage are the daemon-wiring wave's; the TCP leg above runs the same code on
  two in-process daemons over the venue's loopback. `dmesg | grep retained` was not exercised by
  this wave (no FUSE mount in its legs).
- The frame screen's inputs are supplied (`ScreenSource`, the suspension, the stamps) but the
  product loaders (`node_cache`, `smo`) still call `verifyNodeExtentScreened` with `null` —
  threading the per-slot screen and the stamped `NodeLayout` through them is owed with the
  loader binding.
- The dead recoverer's stashed interior records (`recovering_structure`) and the GrowRing pending
  segment settle are not modelled by the RAM seam.
