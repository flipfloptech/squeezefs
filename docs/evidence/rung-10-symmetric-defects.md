# Rung 10 — the symmetric plane's open defects after the tripwire hunt

`docs/evidence/rung-10-tripwire-hunt.md` §5, §5.1 and §8 left four items on the post-perf3 base:
C's `ls` listing one more name than `find` in the shared hot directory row's phase 3, 11 cross-owner
intents still open at A after C's kill in phase 4, `dlm_token_nonmember_refusals` firing in a
passing run, and the owed chunked `extent_grant:` record behind the custody move's refusal past one
tree-0 value. Decisions: 0003 §AE.1–§AE.4; 0002 rows 525–526.

Attribution: all three defects predate perf3. Items 1 and 3's red rows fail on the pre-perf3 base
(`05f9fd2` with the custody fix) as on `dev`, and item 2's fiber is the same code there
(`ReclaimCadence.run`: `xvCadenceTick`, then `wp.tick()`; `cadenceTick` sweeping first). Each
cause is older: the merged readdir's restart came with `cd5a9d3` (2026-10-13), the token holder's
single lease judgement and its unwoken parked grant with `8798f10` (2026-10-08), and the one meta
fiber for the cross-owner passes and the membership work with `a4ca3c0` and `1433a48`
(2026-10-08).

## 1. C's `ls` listing one name more than its `find` (item 1)

### 1.1 The defect

`rung-10-tripwire-hunt.md` §5: in one run of the shared hot directory row on the post-perf3 base
(phase 3, after B's death), C's `ls hot | wc -l` read 10,365 where `find` and A read 10,364. The
row's view compares the two counts from every survivor; one more name from `ls` than from `find` on
the same mount, seconds apart, is a listing that names a file twice.

The merged readdir (`stripe.readdirMerged`, 0003 §U.3) reads `D`'s own stream before its stripes
over every cookie range; a stripe's entry past `D`'s loaded range waits for `D`'s next page, and at
that refill every stripe whose loaded page reached past `D`'s old range is read again — from its
LAST CONSUMED cookie. The range between that cookie and `D`'s old cover had already been answered:
by `D`'s old page and by the stripe snapshots taken after it. A name the lazy migration moved into
that range after the stripe's snapshot (the move inserts into the stripe, then removes from `D`) was
listed from `D`'s old snapshot — and the restarted stripe's fresh snapshot listed it AGAIN, behind
the reply's frontier: out of cookie order, so when it ended a reply the kernel's continuation from
its cookie re-listed what followed it too. The U.3 red row never drained `D`'s page inside one reply
(6-entry replies over 8-entry stream pages); the daemon's replies are a page of dirents (tens to
hundreds of entries) over 8-entry stream pages at K = 64, so `D` drains and refills inside every
reply while it still holds names — exactly while the migration runs. In phase 3 A, the directory's
new holder after B's recovery, migrates the phase-1 names; C's `ls` crossed one of those moves.

### 1.2 The red row and the fix (0003 §AE.1)

- **Red** (`c6bef64`, portable, `stripe.zig` "a reply WIDER than D's stream page lists a name the
  lazy migration moves mid-reply exactly once"): the U.3 fixture (K = 2, 40 names, the odd ones in
  `D`) at replies of 30 and 64 entries — `D`'s 20 names span two stream pages inside one reply —
  every odd name moved before every page read index in turn. On `dev`: a reply out of cookie order
  (the moved name re-listed behind the frontier).
- **Fix** (`1b37957`): a restart re-reads the stripe from `D`'s old cover (`Stream.restart(…,
  cover)`), never from its last consumed cookie: every entry of the range the stream held was
  consumed before `D`'s last (cookie order), a name moved into it was listed from `D`'s snapshot,
  and a create that landed there landed behind the listing (POSIX leaves an entry created during a
  listing unspecified). The range past the cover — unconsumed, ineligible until `D`'s next page —
  is read again as before, so U.3's no-miss argument stands. The stream's consumed-cookie word is
  gone. Both rows green (the U.3 row and the wide-reply row), every stripe row green.
- **Attribution**: the red row fails on `05f9fd2` too. The restart from the consumed cookie came
  with `cd5a9d3` (2026-10-13, "the merged readdir reads D before its stripes over every cookie
  range"), which predates perf3.

## 2. Open intents that never rolled forward after C's death (item 2)

### 2.1 The defect

`rung-10-tripwire-hunt.md` §5: once, in phase 4 of the shared hot directory row, 11 cross-owner
intents stood open at A for the row's whole 120 s wait after C's kill — and no
`xv_cross_owner_intent_stuck` fired, although an abandoned intent older than 45 s fires it at the
register's next publish. So the publish never ran again: the fiber that runs it had stopped.

The mount ran ONE meta fiber for all of its background cadence (`cli/mount.zig` `ReclaimCadence`):
the corpse reclaim, the cross-owner cadence — `Plane.cadenceTick`: the parked guard scopes' expiry
sweep, the register's publish, the roll-forward of abandoned intents, then the stripe migration —
and the cluster wire's tick: evictions at `T_owner`, the grace reap, the death ledger's poll with
the recoveries it acts and, after a recovery, the roll-forward of the recovered slots' intents
(`rollForwardRecovered`). The roll-forward and the migration take the plan's guards in this
process's table with a WAITING `lockManyWide`; a served `XvGuards` from another member parks a
scope in that same table (`guards.ParkedScopes`), released by the member's `XvRelease` — or, for a
member that died holding it, only by the expiry sweep after the owner evicts it. Both the sweep and
the eviction ran on the fiber that was waiting: a pass that met a dead member's scope waited for
ever (and with it every later eviction, recovery, sweep and publish; every op of the process on the
same stripe waited too, until the kernel's request timeout ended the connection).

### 2.2 The red row

Waiting for the 1-in-~100 recurrence would not pin the cause, so the row was made to meet it: phase
4 now storms C too, through its kill (`f839d5f`) — C dies holding scopes parked at A by its own
creates into A's stripes, the contract the row states ("kill -9 C … mid-storm"). On the tree
before the fix (`dev` with items 4, 1 and 3) the row failed **5 of 10** runs (one manual run, 1 of
3 in a first loop, 3 of 6 interleaved with the fixed build; 3 of the 4 under 4 × `yes`, 2 of the
6 alone):
A never evicted C (`appender_recoveries` stuck at 1; A's log has no second `tick — 1 evicted`
line), A's create into `hot` parked at `handler_entry` with `lock_ticked_reregisters` climbing for
50 s until the kernel's 51 s request timeout ended A's connection (`StatsUnreadable` at the
recovery wait). The original failure is the same stall entered later: after C's recovery, with its
11 intents adopted, a pass met a scope C had parked, and the publish (the stuck tripwire's only
caller) rode the stalled fiber.

All five failures carry the same signature: A's watchdog names one `create` (ino 3, the hot
directory) parked at `handler_entry` with `lock_ticked_reregisters` climbing.

### 2.3 The fix (0003 §AE.2, `d2413bd`)

- **The membership cadence on a fiber of its own** (`MembershipCadence`, spawned on a symmetric
  set): the wire's tick, then the parked scopes' expiry sweep and the register's publish
  (`kv_fs.xvSweepTick` → `Plane.sweepParked`). Nothing it runs waits on a DLM guard — it is what
  dooms a dead member's lease and frees the scopes it parked — so a pass on the reclaim cadence
  waiting on such a scope resumes within a tick of the eviction, and evictions and recoveries never
  wait behind a roll-forward's ship timeouts.
- **The poll adopts; the cadence rolls forward**: after a recovery the death ledger's poll hands the
  recovered slots' open intents to the cross-owner register (`Plane.adoptRecovered`: the homes
  scanned, every unknown intent adopted abandoned — reads only, no guard, no dispatch;
  `recovery_intents_adopted`) and the reclaim cadence's next pass completes them.
  `rollForwardRecovered` is gone; `Plane.cadenceTick` no longer sweeps.
- **The plane row** (`two_holders` "a roll-forward waiting on a guard scope a DEAD member parked is
  freed by the membership cadence's sweep, which never waits behind it"): A's abandoned create, a
  scope parked at A over its child by a member the owner lists live, the pass on its own thread —
  the sweep keeps the live member's scope; once the owner lists it dead the sweep releases it at
  once (`swept_dead` + 1) and the pass completes the create; then a new incarnation's
  `adoptRecovered` adopts an intent on the store (counted open, nothing dispatched) and the pass
  completes it.
- **Attribution**: the stall predates perf3. On `05f9fd2` `ReclaimCadence.run` already calls
  `xvCadenceTick` and then `wp.tick()` on one fiber, and `Plane.cadenceTick` sweeps the parked
  scopes before the roll-forward. The parked scopes and the waiting roll-forward came with
  `a4ca3c0`, and the wire's tick joined that fiber with `1433a48` (both 2026-10-08).

## 3. `dlm_token_nonmember_refusals` in a passing run (item 3)

### 3.1 What fires it, and why it is not a fault

The site is the token holder's frame law (`token_table.Holder.serveGrant`): a grant ask from a
client whose lease the membership owner lists EXPIRED is refused `NotMember`, nothing registered.
On the manager the owner's word is its roster: a member is listed and EXPIRED from its deadline
until the next wire tick evicts it — the window the tripwire fired in (twice at A in run 4 of the
tripwire hunt's pristine `f344862` loop, once in a pristine post-perf3 run), each time in a row
that passed.

A member's own clocks keep it from ASKING past its lease (the reader refuses itself at `T_self <
T_owner`, `tokens.zig` "the parked joiner's root"), so an ask served past the owner's deadline is a
frame the member sent while live and the holder served late. The listener serves one frame at a
time per connection; a grant ask parks (by the charter's design) while a recall of the same
`(client, object)` is pending — and a recall to a member that just died completes only at its
lease's expiry. So the killed holder's (B's, then C's) asks queued in A's session behind its own
parked grant: the parked one woke (§3.2's second defect: only an unrelated wake or the eviction
woke it), registered a token for the dead member, and the next queued ask met the expired lease.
That refusal is the protocol's correct answer — a frame from a member that is no longer one — and
it can follow any session delay past `T_owner − T_self`; the reference counts it and warns
(`token_plane.rs` :1690–1721) and its tests expect it (`sym_coherence_tests.rs` :3797, :4640). The
port's must-stay-0 marking (`sym_tokens_intents_striping.md` §5.2) was wrong: a tripwire that
fires in a green run is a defect of the instrument.

### 3.2 The two defects beside it

- **A parked grant registered for an expired member.** The lease was judged once, at the ask; a
  grant that then waited for the member's own pending recall — a wait that ends at the lease's
  expiry when the member stopped acking — registered a token no lease backs.
- **A parked grant was not woken when the holder's pass retired its recall at the lease's
  expiry.** The pass retires an unacked frame in its own locked sweeps (`sweepExpiredLocked`,
  `expireOverdueLocked`) and posted no wake, so a grant parked on that recall waited for an
  unrelated ack, the eviction's departure sweep, or its own park deadline (`T_owner`): the
  member's session stood still behind it for that long.

### 3.3 The red rows and the fix (0003 §AE.3, 0002 row 526)

- **Red** (`64e99c4`, portable, `tokens.zig`): "a grant ask from a member whose lease lapsed before
  its eviction ran … is refused NotMember and counted … never a tripwire" — on `dev` the tripwire's
  error log (the test runner fails it) and `invariant_tripwires` + 1; "a grant ask parked on the
  member's OWN pending recall whose lease lapses during the wait … is refused NotMember at its
  registration" (the member unreachable for pushes, its lease 400 ms from expiry, the pass on its
  own thread) — on `dev` the ask was never woken at the expiry: the client's call answered
  `Deferred` at its reply bound.
- **Fix** (`b95574c`): `dlm_token_nonmember_refusals` is a `ShardedCounter` with a warning naming
  the client (its reason on its declaration); the lease is judged at the ask AND before every
  registration (`Holder.refuseExpired`); a frame the pass retires at a lease's expiry or past the
  deadline posts the ack's wake (`ack_wake.notifyWaiters`) so every grant parked on that recall
  re-judges at once. Both rows green; the ghost-member row keeps its refusal, now counted.
- **Attribution**: both red rows fail on `05f9fd2` too (the tripwire's error log; `Deferred`). The
  holder's grant table with its single lease judgement came with `8798f10` (2026-10-08), which
  predates perf3.

## 4. The grant record as a family of chunks (item 4)

### 4.1 The limit

The custody move of 0003 §AD.1 writes the grantee's `extent_grant:{a}` record gaining a moved
tree's live images in the lease's ONE control entry (T1) and the departing record losing them in
the release's (T5). The record was ONE tree-0 value: `extentGrantMaxRuns(value cap)` runs — 4,111 at
the default 256 KiB node, 1,039 at 64 KiB — and a move past it refused (`Rejected`,
`extent_grant_custody_cap_refusals`): the lease did not move. A tree a joiner did not grow carries
its images scattered (the holder's SMOs allocated them among everything else), so a slot whose tree
holds more than ≈ 4 k scattered images could never be handed over or released into a lessee's
record — an availability limit a long-lived slot meets at scale. The same cap cut carves and refused
returns that would split a run past it (`extent_return_run_cap_refusals`).

A record value is not the only bound: one control entry carries at most `MAX_ENTRY_LEN` = 128 KiB,
so even a record split into several values cannot be rewritten whole in one entry once it passes
≈ 8 k runs, and a release entry that already carries an inline `slot_tails` record near the value
cap had no room for a 64 KiB record beside it (the entry refused `ValueTooLarge` → `Corrupt`).

### 4.2 The format and the law (0003 §AE.4, 0002 row 525)

- **Key** `kind ‖ tag 3 ‖ appender u32 BE ‖ id u64 BE` (14 B): one appender's chunks are ONE range
  walk (`cr.extentGrantFamily`), the move marker at id `u64::MAX` sorts last.
- **Value** `ExtentGrantChunkHead` (48 B: version 2, kind runs/marker, `n_runs`, the key's appender
  and id repeated, a marker's staged range, the `checksum.stamp` over the whole value) ‖ 1 … 253
  `GrantRunWire` (16 B each). 253 runs = one 4 KiB value: one journal page, the grain every entry
  pays — and below the smallest node's value cap (16,640 B at 64 KiB; `grant_record.zig` asserts it
  at comptime). The decoder (`decodeGrantChunk`) refuses a stamp that fails, a head naming another
  key, a non-zero reserved word, an empty, unnormalized or over-long run list, a marker away from
  its id or staging an empty range; it is total under the tree-0 fuzz target (the census rail).
- **The record** is the union of the effective chunks — pairwise disjoint, adjacent runs across a
  chunk bound coalesced (`grant_record.Family.load`).
- **A change is a replacement** (`Family.plan`): each effective chunk owns the extent range from its
  first run to the next chunk's first run (the first chunk from 0); the target run set is cut at
  those bounds and every chunk whose share changed is deleted, its share written as fresh chunks
  under ids the family never used. A carve or a return touches one or two chunks; a custody move
  touches the chunks its images fall in.
- **A replacement that does not fit its change's entry is STAGED** (`grant_record.Writes`): the
  first staged entry writes the MOVE MARKER naming the fresh ids `[lo, hi]` — a chunk inside a
  present marker's range counts for no reader — the fresh chunks follow in entries of half the
  entry payload (`stagedEntryRoom`), and the change's own entry (the slot state, the carve's
  `alloc` deltas, the return's frees) deletes the replaced chunks and the marker. Before that entry
  lands every reader sees the old record; at it, the whole new one. A change whose own entry never
  lands leaves the marker: the next load of that record (`Manager.readGrantRecord`, the recovery's
  `loadGrant`) deletes its staged chunks, then the marker (`extent_grant_markers_swept`, a warning);
  readers meanwhile never count them. Counters `extent_grant_staged_changes`,
  `extent_grant_staged_chunks`.
- **The envelope**, stated: the change's own entry carries one 32 B delete per replaced chunk —
  ≈ 2,000 chunks (≈ 500 k runs touched) beside the largest change record; past it the change
  answers `Rejected`, loud. A carve is cut to `carveExtentsMax()` = 2,298 extents: each claimed
  extent costs its `alloc` frame and, at worst, one chunk delete in the entry that lands it (the
  old run-cap cut, now the entry's own bound; the old code's single entry overflowed at ≈ 4 k
  claimed extents anyway).
- **Readers**: the manager (`readGrantRecord`, `heldElsewhere` over every family), the recovery
  (`Volume.grantFamily`: step 3's screen and kept-extents, step 7's rewrite as one replacement —
  staged when large; a re-run after a crash between its entries sweeps the marker, so the record
  never names less than the old or more than the new), the volume binding's return census, and
  fsck: the forest census reads every volume's grant records whole and a family that does not
  assemble is `C14GrantRecordCorrupt{vol, appender}` (report-only — the record is the manager's
  only attestation of the grant).
- The refusals are gone: `extent_grant_custody_cap_refusals` and `extent_return_run_cap_refusals`
  are retired with the run cap.

### 4.3 The red row and the rows beside it

- **Red** (`c372f34`, portable, `slots/acquire.zig` "a slot whose tree holds more images than one
  tree-0 value carries MOVES"): the manager's RAM volume at 64 KiB nodes; slot 5's tree unleased
  under a root; a `treeImages` seam answering 1,500 scattered extents (every other one of
  [1000, 4000)); appender 2's `AcquireSlots`. On `dev`: `error.Rejected` — the custody cap. Green
  (`3046b79`): the grant lands, `extent_grant_images_transferred_in` + 1,500, the record holds
  exactly the even extents in ⌈1,500 / 253⌉ = 6 chunks, and appender 2's `ReleaseSlot` takes all
  1,500 back out (`_out` + 1,500, the family empty).
- `manager` "a custody move whose chunks do not fit the lease's entry is STAGED": 1,400 scattered
  images against an entry with 8 KiB of room — the marker and the six fresh chunks land first and
  the record reads exactly as before them; the lease's entry makes all 1,400 effective at once; a
  second move whose entry never lands is swept (`abandonStaged`) and the record stands as it was; a
  marker left behind is swept by the next verb's read.
- `grant_record` (RAM tree 0): a replacement rewrites only the chunks whose share changed (one run
  grown: one delete, one put), ids never reused while the family stands, the family gone when the
  record empties; 12,000 scattered images staged across entries each within the staged room, no
  reader counting a staged chunk until the final entry; a family that does not assemble is
  `Corrupt` (overlapping chunks, a chunk under another appender's key, a failing stamp); adjacent
  runs across a chunk bound read as one.
- `control_records` "extent_grant chunks": round trip, the key binding, every refused spelling
  byte-wise even re-stamped, the marker, the 253-run tie; `fsck` "C14 reads every grant record
  whole"; `extent_grant` "subtractRuns".

## 5. Loops and gates

The shared hot directory row alone (`-Dtest-filter="SHARED HOT DIRECTORY"`, ≈ 40 s a run) under
the harness of `rung-10-tripwire-hunt.md` §5: odd rounds alone, even rounds under 4 × `yes`, on the
venue the other worktrees' runs share (load average 4–13). A run passes when the row passes: every
survivor's `ls` equals its `find` and its `stat` walk at each phase, and no cross-owner intent stays
open after either death. A run's daemon logs are kept when it fails or logs a tripwire.

| Build | Alone | Under 4 × `yes` | Tripwire runs | Note |
|---|---|---|---|---|
| `d5e3703` (the post-perf3 base, the custody fix, the row's listing capture; none of items 1–4) | 15 / 15 | 15 / 15 | 0 | items 1–3 did not recur in 30 runs |
| the same, a second loop | 6 / 6 | 4 / 5 | 1 | the failure: A's `SIGABRT` at `fuse/transport/session.zig:642` (`Sideband.completeCell`, owned by `.wt-coherence`), `appender_park_expiries` firing after it |
| items 4 and 1 (two loops side by side) | 18 / 18 | 18 / 18 | 0 | |
| item 2's red row, without the fix | 4 / 6 | 1 / 4 | 0 | §2.2: every failure is A's wedged `create` |
| item 2's fix before the rebase, interleaved with the red row | 4 / 4 | 3 / 3 | 0 | |
| **the landed tree, `168b495`** | **60 / 60** | **59 / 60** | **0** | the failure (round 76): A's mount could not arm — `kmbuf ring registration ENOMEM (order-8 contiguous pages)` at startup, before the row's first op; the transport's class, owned by `.wt-coherence` |

Gates before each landing:

| Landing | Linux (`zig build gate`) | macOS (`zig build gate`) |
|---|---|---|
| items 4 and 1 (`af314c2`, `5eba3e1`) | 49/49 steps, 2,866 / 2,939 passed (73 skipped), 6 m 38 s | 29/29 steps, 2,255 / 2,284 passed (29 skipped), 3 m 33 s |
| items 3 and 2 (`b54bef9`, `168b495`) | 49/49 steps, 2,873 / 2,946 passed (73 skipped), 8 m 26 s beside a loop | 29/29 steps, 2,234 / 2,263 passed (29 skipped) |

Two macOS runs on the way failed one unrelated row each and were green on re-run: the write
pipeline's admission row, and fsck's "zero false positives" row (the pre-existing loaded-host flake
of `rung-10-tripwire-hunt.md` §8).

## 6. Owed (seen beside this work, older than it)

The first Linux gate on this note's commit failed two rows. Neither touches items 1–4:

- **The joiner rejoin row** (`smoke.zig` "A JOINER'S CLEAN UNMOUNT AND REJOIN") fails at a fill
  with one of two signatures: B's `no heap extent for slot N's first tree (heap full)` (the fill's
  `dd` answers EIO), or B's next join refused (`JoinAppender refused for capacity`; the joiner maps
  the manager's `no_space` to capacity too, `join_ladder.zig` :213). The row alone, interleaved,
  half of the rounds under 4 × `yes` on a loaded venue: `d5e3703` (before items 1–4) failed 4 of 14
  runs, the landed tree 2 of 13. Both signatures are a heap extent that no grant could supply when
  a fill or a join needed one (each new slot's first tree takes a fresh extent); the cause is not
  pinned here.
- **`nvmet fidelity: sigstop-preempt`** read `writer_guard_verdict` `healthy` beside
  `writer_guard_fenced` 1. The counter is process-wide (`journal.FailureLatch.noteFence`) and the
  verdict is one lane's latch (`guard/gate.zig` :155): when another volume's lane met the
  reservation conflict first, the stats snapshot can show the counter before the gate's own latch.
