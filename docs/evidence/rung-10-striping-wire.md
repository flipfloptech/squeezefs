# Rung 10 — directory striping end to end over the wire (items 1–3)

Charter §5.5 "Directory striping" (K = 64, the census-triggered flip and the
explicit command, the lazy migration, C17), the holder-decided handover under
`N_floor`, cross-owner intents and their roll-forward, `record_ship`, and §5.9
death and recovery. The owed item this note closes is
`docs/evidence/rung-06-symmetric-plane.md` "Not done": "the shared-dir
handover/striping matrix row: the holder-decided handover under `N_floor` and
the K = 64 striping engage on a hot directory two writers share … the two-daemon
row that drives a shared directory past `N_floor` and asserts the offer, the move
and the stripe flip was not run". Items 1–2 made the flipped directory a
filesystem and carried its two wire halves; item 3 is that row, and the fourteen
product defects it found. Decision rows: 0002 rows 493–495 (items 1–2) and
501–505 (item 3); 0003 §Q.1–Q.10 (items 1–2) and §Q.11–Q.20 (item 3); the
spec's quirks Q20–Q27 (`docs/porting/sym_tokens_intents_striping.md` §7). Venue:
the macOS `container` (`7.2.6-sqz`, 9 vCPU, 16 GiB), shared live with the
`.wt-umount` and `.wt-staging-scope` agents' gates.

## 1. `kv_fs` over a FLIPPED directory (item 1) — landed `1e1b1bd`

**Design.** Every dentry op of `kv_fs` runs the striped PRELUDE
(`fuse/handler/kv_fs_stripes.zig`): the directory's verdict (plain / striped
with its map / a stripe) off a cache whose positive entries stand until the
directory's `rmdir` (a map is immutable, a stripe ino never reused) and whose
plain hints are the HOLDER's, keyed by the slot's lease generation (0003 §Q.7);
a stripe is known by its record's `INODE_FLAG_STRIPE` wherever the record is
read (§Q.1, 0002 row 493 — the reference's reverse scan needs a parent memo a
holder that acquired the slot by handover or recovery never has). Lookup reads
the name's stripe (`hash54 % K`) first, then `D`'s own tree (a name not yet
migrated); create/link key the stripe with the EEXIST screen over `D` and the
dying-stripe refusal; unlink/rename re-home a name still in `D` before the
mutation; the logical parent stays the `..` memo's and the ancestry walk's;
`readdir` is the K-way merge in cookie order at the FUSE boundary; `stat D`
folds nlink and times; `rmdir D` is ONE intent (the widest image,
`intent.IMAGE_MAX`, §Q.9). The explicit flip is the `setxattr
user.squeezefs.stripes` COMMAND (`getxattr` answers the map's K, ENODATA on a
plain directory, §Q.10); the census-triggered flip hands its map back to the
cache; the migration moves one page of names per directory per cadence tick
(§Q.8). The census' creator is the shipped child's slot holder, and a striping
domain feeds no census and no handover window (§Q.4).

**Red tests.** `kv_fs_stripes` "kv_fs over a FLIPPED directory (RAM store)"
(K = 64: the route, lookup, a readdir restarted mid-stream, the fold, a rename
across stripes, the one-intent rmdir); the two-member set row (the flip's
intent, a stripe-to-other-member rename as one intent); the plan row at the
widest image. Smoke stage "A STRIPED DIRECTORY ON ONE DAEMON" (the explicit
flip, 10,000 creates keyed to their stripes, `ls`/`find` equal, a 1,000-move
rename storm across stripes, the fold, `rm -rf` as one intent, `fsck` 0
findings).

## 2. The wire halves (item 2) — landed `1e1b1bd`

**Design.** `SupplyStripeInos` (0x0306) is ONE batch per creator at the share
`K / (creators + 1)` — the served side mints flagged stripe records in its own
slot on the directory's volume; a declined or unreachable creator leaves its
share to the holder (§Q.2, 0002 row 494). The explicit flip from a non-holder
ships as the `setxattr` COMMAND over `record_ship` and the holder's service runs
it (§Q.3). The foreign-slot LAYOUT publish rides `record_ship` with the
writer's DMA proofs (`RecordShipHead.n_proofs` + `DmaProofWire[]`); the holder
stages the record AND its ±ref diff through the data plane's `PublishFace`, a
block without its proof is `ProofMissing` ⇒ rejected, and a publish naming an
inode with no record is refused (§Q.5, 0002 row 495). A foreign-slot file is
always written through the striped arm (§Q.6).

**Red tests.** two_holders "striping: a flip supplied by the other creator
OVER THE WIRE" (one batch for its share, K stripes under ONE intent, every
stripe record flagged, a create keying a foreign stripe shipped, the lazy
re-home, the K-way readdir with a stable continuation across the flip, the
fold, the one-intent rmdir, C17 clean); the record ship's publish with proofs
rejected without a face. Smoke: the two-daemon matrix's foreign write (8 MiB
from A into B's file: 3 publishes shipped = served, the sha equal on both).

## 3. The shared hot directory across daemons (item 3)

### 3.1 The row

`smoke: THE SHARED HOT DIRECTORY ACROSS DAEMONS` (`src/cli/smoke.zig`):
three real FUSE daemons over TCP — A the manager, B and C joined, each in its
own UTS namespace — `T_owner` 3 s (the smoke's seam), `N_floor` 17 (the
manager's derived floor on the venue).

- **Phase 1 — the handover.** A mkdirs `hot`; B alone storms creates into it
  (each one shipped insert served at A). B dominates the slot's window past
  `N_floor`, A OFFERS, the offer rides B's renewal grant, B accepts, A's
  flush-then-transfer moves the slot. Asserted: `slot_offers_pushed` ≥ 1 and
  `slot_handovers` ≥ 1 at A, `joined_wire_offers_accepted` ≥ 1 at B,
  `dir_stripe_flips` 0 at A (one creator), steps served at A; a probe create
  from A then ships to B.
- **Phase 2 — the flip.** A and C storm into `hot`, now B's. Two creators past
  `N_floor` at B ⇒ exactly ONE flip over K = 64: `SupplyStripeInos` 2 RPCs
  supplying 2 × 21 stripes (one batch from A, one from C,
  `dir_stripe_supplies_served` 1 at each), 22 minted by B, 0 declined, 0
  dying-stripe refusals; no further handover (a ship into a striping domain
  feeds no window); stripe ships from A ≥ 1. Then `ls hot | wc -l`, `find hot
  -type f | wc -l` and a sha256 over `stat(2)` of every entry (`%n %i %h %s`:
  name, ino, links, size) are byte-equal from A, B and C and equal the storms'
  exact count.
- **Phase 3 — the death.** A and C storm again; kill -9 B — the holder of
  `hot` and 22 stripes. A evicts B at `T_owner` and recovers its region
  (`membership_evictions` 1, `appender_recoveries` 1, `appender_rollbacks` 0,
  `recovered_regions_released` 1); the storms' refused creates retry (a retry
  that meets the name counts once). Every open intent rolls forward:
  `xv_cross_owner_intents_open` 0 and `xv_cross_owner_intents_stuck` 0 on A
  and C; the survivors' views are equal, `ls` = `find`, every counted name is
  listed and at most one more per storm. Umount C, A; `fsck --offline --scrub`
  0 findings.

The storms are process-group leaders the row kills on every exit path
(success, a failed expectation, a timeout), and each loop also ends when the
test process is gone (§Q.17): an earlier draft left six writer loops spinning on
a detached mount with parent = init. `pgrep -fc 'true > "hot/'` read 0 after
every run below. The view script lands `ls`/`find` in the work directory first,
so a failed listing fails the script with its stderr — before, `sh -e` judged
the pipeline by `wc` and a daemon answering EAGAIN for a whole directory counted
0 entries silently (that is how defect 8 hid).

### 3.2 What it found

Fourteen product defects, in the order the row met them; every fix landed with a
red unit row first.

| # | Defect, as the row met it | Fix | Red row | Commit |
|---|---|---|---|---|
| 1 | Phase 1: the accept was refused and the handover undid itself — in the release → grant gap B's own creates, still shipping to the releasing A, first-touched the slot back at A | The released slot is a RESERVATION for its accepter: other first touches refused naming it (`slot_acquire_reserved_refusals`), the rotor skips it, the accepter's grant clears it, a lapsed stand frees it (0002 row 501, §Q.11) | acquire "the release → grant gap of an accept" | `f79abbf` |
| 2 | Phase 2: no flip — on a joiner a THIRD appender's slot read as the manager's, every step bounced `not_holder`, and B's census credited both creators to one | A joiner asks the manager ONCE for a slot it knows nothing of; `unleased` answers remembered (`known_unleased`) (§Q.12) | resolve "a joiner's knowledge of a slot beyond its table" | `4e7bdb7` |
| 3 | Phase 2: every create into `hot` parked 45 s at the flip — the census flip ran inside the served insert whose initiator's scope held `I{D}` | The served insert only queues the due directory; the xv cadence flips it, failures named and counted (`dir_stripe_flip_failures`) (§Q.13) | two_holders "the automatic trigger" (due, nothing inline; `runPending` flips) | `db3c802` |
| 4 | Phase 2: A's session to B wedged until the grace belt — a served `XvGuards` parked on a stripe another scope of the SAME initiator held, and one session per peer serves one frame at a time | The served acquisition takes the TRY form and answers `Busy`; the initiator retries with a doubling 1 → 16 ms backoff under the reply bound (0002 row 502, §Q.14) | two_holders "the served acquisition never parks the holder's session" | `3638c3b` |
| 5 | Phase 3: every op on the dead B's stripes parked 45 s at C — no holder's parked guard scopes had a membership word (the seam was never bound; the client was keyed by the writer id) | A joiner's census view (the owner's census per renew interval, `grace_open` on the page, the `since` law); the dead member's scopes swept at once (`xv_cross_owner_guards_swept_dead`) (0002 row 503, §Q.15) | census_view "the census view"; guards "the parked scopes", "clientOf"; wire_plane "client ids" | `0a68d2b` |
| 6 | Phase 3: C's served `xv_step` took 45,000 ms per pass and A's calls timed out behind it until A's intents stood past the TTL — C's token holder waited each recall to the dead B out to the deadline | `LeaseOracle.verdict(client, since, now)` with the reader's last grant as `since`; the census commit sweeps the holder and wakes a waiting pass; the wait reads the owner's word after arming the wake (§Q.15) | tokens "a joiner's holder and a reader the owner evicted" | `ecb9b4e` |
| 7 | Phase 3: A dialled the dead holder of a recovered stripe until its intents stood past the TTL — a step gave up on an unreachable holder without re-resolving | `unreachable_` and a failed ship are the slot-moved class: re-resolved up to `SLOT_MOVED_RETRIES`, then left open, abandoned counted once (§Q.16) | two_holders "a holder that cannot be reached is re-resolved before the ship is given up" (proven red against the pre-fix arm) | `fe2ebe8` |
| 8 | Phase 3: C's `ls hot` answered EAGAIN for the dead B's former stripes long after A recovered them (silently 0 entries before the view script judged `ls` itself) — a token read never re-resolved an unreachable holder | One re-resolve per op through `Resolver.refresh`, shared with the redirect's follow (`dlm_token_reader_unreachable_resolves`) (§Q.16) | read_divert "a holder that cannot be reached re-resolves the slot ONCE inside the op" | `ec3f3bf` |
| 9 | Phase 3, 1 run in 3: `fsck` `[C9] live (nlink > 0), prior-era, named by no dentry` — B answered a served insert before its window landed, the creator retired its intent, the dentry died with B's ring | Every served mutation (step, record verb, stripe supply) barriers the holder's ring before the reply — the spec's §2.3 law (§Q.18) | two_holders "a served mutation's reply follows the holder's durability lane" | `f492105` |
| 10 | Phase 3, 2 runs in 8 with fix 9 in: `[C9]` again — a create's ship failed against the corpse, the shell retried the SAME name and won it after the recovery, and the roll-forward skipped the refused insert and retired, orphaning the first child | A roll-forward that re-meets a live refusal compensates (the inverses of the pass's steps and of fresh mints found applied) (`xv_recovered_compensated`; 0002 row 504, §Q.19) | two_holders "an abandoned create whose name a retry took meanwhile is COMPENSATED by the roll-forward" | `809a226` |
| 11 | Phase 3, 2 runs in 9 with fixes 9–10 in: C listed 12,077 names where A listed 16,536 — C's cached dentry windows of B's former stripes, frozen at the death: a dead holder's grants are recalled by no one | The census commit names its departures with their listeners; every reader plane bound to a departed listener is re-pointed, every token dropped (`joined_dead_holder_planes_dropped`) (§Q.15) | census_view "the census view's departures" | `e355585` |
| 12 | Phase 3, 10 runs in 10 once fix 11 stopped serving the corpse's cached tokens: C's `ls hot` EAGAIN — a refused first touch had loaded B into C's lease table, and the manager's `unleased` dropped only the learnt view | `unleased` also drops the table's projection of a foreign holder (`LeaseTable.dropForeign`), never an own lease (§Q.16) | resolve "a joiner's table drops a FOREIGN lease the manager answers unleased" | `51237de` |
| 13 | Phase 3, 2 runs in 10 with fixes 9–12 in: `stat hot/b415` ENOENT at C for a name `ls` had just listed — A (the directory's new home) was migrating the phase-1 names, and the lookup read the stripe (before the move's insert) then `D` (after its remove) | A miss in both reads the stripe again: every move inserts into the stripe before it removes from `D` (`stripe.locateName`; 0002 row 505, §Q.20) | stripe "locateName: a name the lazy migration MOVES between the lookup's two reads" | `abeee1e` |
| 14 | The first full gate on fixes 1–13, the handover row (rung 10 item 4): B's `chmod` of A's file answered EAGAIN between A's transfer and B's grant — fix 1's reservation refused every touch but the accepter's, while the resolution still sent them to a door that could not take the slot | A reserved slot routes to its accepter: `holderOf` and `ResolveSlot` name it while the stand holds, so the accepter applies and everyone else ships to it (`LeaseTable.stands`, a lock-free gate a joiner's table never opens) (§Q.11) | resolve "a slot a handover released stays its ACCEPTER's for routing while the stand holds"; acquire "the release → grant gap" (`ResolveSlot` answers the accepter) | `595a16b` |

The wedges of defects 3, 4 and 6 were silent: a served frame parked for 45 s and
the dialer's callers timing out at the 10 s reply bound left no line in either
log, only overdue FUSE ops elsewhere. The listener now names a serve that held
its session past `SLOW_SERVE_WARN_MS` = `DIAL_TIMEOUT_MS / 10`
(`cluster_wire_slow_serves`), and the dialer's give-up names the verb and the
peer (`9c95c6c`).

### 3.3 The ten runs

Ten consecutive runs of the row on the landed code (`zig build smoke
-Dtest-filter="SHARED HOT DIRECTORY"`, one after another on the shared venue;
the first includes the build). Every run: `slot_offers_pushed` 1 and
`slot_handovers` 1 at A after phase 1 and still 1 after phase 2, `dir_stripe_flips`
0 at A; at B one flip, K 64, 42 supplied (21 + 21 in one batch each), 22 minted;
evictions 1, recoveries 1, rollbacks 0; open intents 0 and stuck 0 on A and C;
`fsck --offline --scrub` 0 findings; `pgrep -fc 'true > "hot/'` 0 after the run.

| run | rc | s | phase 1: B's creates, move ms | offers / handovers | flip ms after the storms | K / supplied / minted | phase-2 entries (A = B = C) | recovery ms | phase-3 entries (A = C) | open → 0 ms | fsck inodes, s, findings |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 0 | 86 | 467, 222 | 1 / 1 (still 1) | 4595 | 64 / 42 / 22 | 3801 | 9015 | 21622 | 18 | 21688, 52.845, 0 |
| 2 | 0 | 72 | 485, 219 | 1 / 1 (still 1) | 4624 | 64 / 42 / 22 | 3443 | 13590 | 18781 | 20 | 18847, 42.163, 0 |
| 3 | 0 | 73 | 490, 432 | 1 / 1 (still 1) | 4410 | 64 / 42 / 22 | 3711 | 8940 | 19561 | 19 | 19627, 47.956, 0 |
| 4 | 0 | 63 | 344, 436 | 1 / 1 (still 1) | 4438 | 64 / 42 / 22 | 2857 | 8880 | 17948 | 20 | 18014, 37.31, 0 |
| 5 | 0 | 71 | 917, 432 | 1 / 1 (still 1) | 4464 | 64 / 42 / 22 | 3590 | 13584 | 18462 | 20 | 18528, 39.508, 0 |
| 6 | 0 | 63 | 410, 438 | 1 / 1 (still 1) | 4407 | 64 / 42 / 22 | 3188 | 9157 | 18224 | 19 | 18290, 37.229, 0 |
| 7 | 0 | 58 | 502, 438 | 1 / 1 (still 1) | 4243 | 64 / 42 / 22 | 2859 | 13258 | 16403 | 17 | 16469, 29.006, 0 |
| 8 | 0 | 66 | 561, 221 | 1 / 1 (still 1) | 4599 | 64 / 42 / 22 | 4056 | 8858 | 18874 | 19 | 18940, 41.905, 0 |
| 9 | 0 | 71 | 476, 221 | 1 / 1 (still 1) | 4611 | 64 / 42 / 22 | 3588 | 9079 | 19995 | 20 | 20061, 44.955, 0 |
| 10 | 0 | 73 | 575, 220 | 1 / 1 (still 1) | 4607 | 64 / 42 / 22 | 3306 | 9337 | 21721 | 18 | 21787, 48.396, 0 |

### 3.4 The earlier reds and their attribution

- **Loop a** (before fixes 9–12; source edits landed mid-loop): run 1 green;
  run 2 the product binary's compile killed by the venue's global OOM killer
  (two other agents' LTO builds in flight — infrastructure, not the row); run
  3 `[C9]` → defect 9. Stopped after run 6.
- **Loop b** (fix 9 in): 7 of 10 green; runs 3 and 9 C's stale view → defect
  11; run 6 `[C9]` → defect 10.
- **Loop c** (fixes 9–11 in): 0 of 10 — C's `ls hot` EAGAIN in every run →
  defect 12 (fix 11 stopped hiding it behind the corpse's cached tokens).
- **Loop d** (fixes 9–12 in): 8 of 10; runs 7 and 9 `stat` ENOENT of one
  phase-1 name at C → defect 13. Eight diagnostic runs afterwards (with a
  failure probe in the row, not landed) were all green — the window is the
  two reads' microseconds.
- **Loop e** (fixes 1–13): 10 of 10. The full Linux gate that followed
  failed the handover row (rung 10 item 4) → defect 14, and once aborted
  daemon A in this row's recovery (owed below).
- **Loop f** (the landed code): the ten above.

Every red of the row but two reproduced a defect of its own paths, each pinned
by its red unit row before the fix; the two others were the venue's OOM killer
(loop a, run 2) and the recovery abort owed below (code paths of `dev`'s that
this landing does not touch).

### 3.5 Measured (venue scoping, not the box)

The slot moved 219–438 ms after B's storm began; the flip landed
4243–4624 ms after A and C's storms began (the census' half-window
pair); A recovered B's region 8858–13590 ms after the kill (`T_owner`
3 s, the eviction tick, the recovery); open intents read 0 within
20 ms of the storms stopping; `fsck --offline --scrub` walked
16469–21787 inodes in 29–53 s.

### 3.6 Not done — owed, precisely

- **The coherence window between a dead holder's recovery and a joiner's next
  census pull.** C drops the dead holder's tokens at the census commit that
  retires it (≤ one renew interval after the eviction). The manager's recovery
  may complete inside that interval, and a write at the manager to a recovered
  object before C's pull is not seen by C's cached entry until the pull. In the
  row the pull lands long before the recovery (renew ≈ 265 ms against a
  recovery landing 6–11 s after the eviction); at the shipped `T_owner` (45 s, renew
  15 s) the window can approach the renew interval. The fix owed: hold the
  release of a dead holder's recovered slots one renew interval (+ skew) past
  the eviction — the custody quarantine's shape — or carry the death on the
  renewal grant.
- **Supply durability** (fix 9's third arm) is pinned by its unit row only: the
  row kills B, never a supplier.
- **A dead MANAGER as the shared directory's holder** is the rung-6 kill rows'
  (park-and-reclaim), not re-run in the striped shape.
- **The roll-forward's compensation attributes only fresh mints** (§Q.19): a
  `link` abandoned after its nlink step and refused at the roll-forward leaves
  that nlink (the C10 class). The row runs no link storms.
- **A recovering manager's pending-free FIFO met a non-monotone gate — once
  in about 75 runs of the row on this branch** (the first full Linux gate,
  on fixes 1–13; in no loop): daemon A aborted while recovering B's region,
  `alloc_core.ExtCore.freePending` (via `alloc_ext.freePending`) from
  `smo.smoReplace` ← `SmoContext.maintain` ← `Checkpointer.flush` ←
  `drainMaintenance` ← `CheckpointTask.runOnce` — one of the core's three
  asserts (extent in range, extent allocated, `last_retire_seq` ≤ the
  gate); the frames above it are inlined. The FIFO is single-producer by
  design ("the serialized task is the FIFO's only producer") and every
  producer found holds `smo_lock`, so the second producer or the foreign
  gate space during a dead joiner's recovery is not identified. None of
  the paths is touched by this landing; the row is the first to recover a
  dead joiner holding a large striped directory under load. Owed: a red
  row that recovers a dead joiner's slot trees while the checkpoint task
  drains (the assert's frame pinned in a Debug daemon), then the fix.
- **The manager keeps a dead appender's endpoint row** (`Manager.forgetEndpoint`
  runs on an in-process leave only): the read and step re-resolves go through
  `ResolveSlot`, so nothing here depends on it, but `ResolveEndpoint` still
  names the corpse's port after its recovery.
- **The merged readdir has the lookup's window across its streams**
  (§Q.20): a name moved out of `D` between the refill of its stripe's
  stream and the refill of `D`'s can be missing from one listing. Not met
  in the runs (every survivor's listing equalled the other's); the owed
  fix is to refill `D`'s stream before any stripe stream over the same
  cookie range and drop the second copy of a name at the merge.
- The row's phase-2 progress line says "(name, ino) digest"; the digest covers
  `stat(2)` of name, ino, links and size.

## 4. Gates

- **Linux** (`timeout 3900 zig build --cache-dir /build/zig-cache gate
  --summary all` in the venue, load < 3, on the landed code before its last
  rebase onto `dev`): 46/46 steps, 2,711/2,776 tests passed (65 skipped) —
  unit 2,495 pass / 63 skip, smoke 82 pass / 2 skip (this row: 23,644
  entries after the death, fsck 23,710 inodes, 0 findings), bench-smoke 41,
  shim-smoke 40, nvmet-smoke 53, rails 16. The three full gates before it
  each met something: gate 1 — this row's daemon A aborted in its recovery
  (the owed `freePending` assert above) and a race in this branch's own
  unit row (fixed: the census sweep and the pass's loop head may each
  retire the frame first, and the row no longer asserts which); gate 2 —
  the handover row (defect 14, fixed) and the online fsck row (the flake
  `docs/evidence/rung-10-umount-busy.md` §6 notes for its owner); gate 3 —
  `ops.jobs.wire` "a fleet READ shard whose worker dies mid-walk", which
  fails 2 of 30 standalone runs on pristine `dev` (`7c63769`) and 3 of 30
  on this tree (`dev`'s timing, untouched here), every smoke row green.
- **macOS** (`zig build gate`, on the landed tip): 28/28 steps, 2,143/2,170
  tests passed (27 skipped; unit 2,035 pass).
- **After the last rebase** (`dev` moved by `fe1ba10`, `src/mem/budget.zig`
  only): the Linux unit suite on the landed tip, 2,498 pass / 63 skip.
- The literal rail's allowlist was rebased per commit (line shifts only);
  every commit passes `zig fmt --check` and the rails on its own.
