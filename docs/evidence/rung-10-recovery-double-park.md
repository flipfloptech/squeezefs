# Rung 10 — the kill matrix's double park: the member's beat waits on nothing else (2026-10-24)

The finding: under load "B parks twice, the second reclaim answers `UnknownLease`, B is poisoned". Venue:
this workstation, `7.3.0-1.7-mainline-sqz`; branch `fix/second-reclaim-unknown-lease` on dev `aa15cbf`.
Scratch `~/tmp/unknownlease/`.

**The sightings.** Gate 102 (`~/tmp/gate102-g753.log:1240-1264`) is the F4 row's OWN trace (`A LIVE MEMBER
EVICTED AT T_OWNER …` pins the double park with `BEAT_HOLD`; shard 10/16 6/6 passed; the gate's red was
ONLINE FSCK). The reader agent's load-loop logs are gone; its load ran that row. No unseamed
`UnknownLease` in any retained log since `c3e53e2`. The real ones: the DEAD MANAGER hang (F4 named the
trigger and left it), and C "parked at `T_self` … and was evicted" in THE SHARED HOT DIRECTORY row
(`d7fcd62` answered with a 7 s `T_owner`).

## Which of the three

1. **Yes: a real eviction at `T_owner`.** The seam-pinned trace (the new row on the seam-only commit):
   B parks at A's kill → A remounts (term 2) → `admitted the reclaim` → the follow-up holds → B renews 0
   times; its committers park the gate at `T_self` 784 ms in (the second park) → A: `tick — 1 evicted at
   T_owner` 4,979 ms in, the death recorded, the region recovered → released: `UnknownLease` → `POISONED`.
2. **No.** The reclaim re-arms `T_self` at its own send instant before it releases the gate (`learn`
   before `release`); a second park while the successor holds the lease re-points (granted, no refusal).
3. **Hence yes, and honest.** By then the region is recovered (slots `Unleased`, page `Recovered`); a
   granted reclaim would be F4's zombie writer. B owes the fail-stop (charter §5.5; the reference's writer
   is terminal too, M:3793-3799). A clean in-place re-join of a member with no un-landed state is a NEW
   mechanism — a fresh `JoinAppender` and region, the forest re-opened under a live FUSE session: the join
   path, an owner call, not built here.

**The root.** The joiner's beat (`sqz-member`) made every member wire duty inline — the census pull each
renew interval, a reclaim's re-point and re-publication, the set-config learn, a carried offer's accept
(`AcquireSlot` + the install) — each bounded only by the dialer's 10 s reply bound against the smoke's 3 s
`T_owner` (`T_self` 797 ms). One slow call to a freshly remounted manager stood between two renewals.

## The law and the fix (`13e3334`)

The beat renews (parked, it reclaims) and judges the park's expiry — nothing else: `T_self` is the
member's clock, and its home shard evicts at `T_owner` only a member that stopped renewing. A follower
(`sqz-member-fol`) owns the census, the post-reclaim re-point and re-publication (a Release count), the
set-config learn (once per grant) and the carried offers (queued; a release notice stays a local eviction
on the beat). `4d9ae87`: THE KILL ROWS (b) awaits the follow-up before counting the re-point; the 7 s
`smoke_storm_t_owner_ms` goes — the storming rows run at 3 s again.

**The join (`aa15cbf`, 0003 §BM)** shares no code: it acts at the joiner's open and the manager's plane
arm; this acts after the open, on the member's threads. They meet at the reclaim's dial, which re-resolves
the manager off the projection §BM keeps current. The re-join (3) would be the join path itself.

## Red tests (red on `638b9a6` = dev's beat + the seam, on `aa15cbf`)

- smoke `A LIVE MEMBER'S SLOW FOLLOW-UP NEVER COSTS ITS LEASE …`: the DEAD MANAGER shape, then
  `SQUEEZEFS_TEST_MEMBERSHIP_FOLLOW_HOLD` holds the follow-up through 2 × `T_owner`; B renews on, A evicts
  nobody, B parks once; released: ls = find, digests equal, fsck clean. Red: the trace above.
- unit `linux: THE MEMBER'S BEAT WAITS ON NOTHING ELSE …` (two planes in-process): red
  `LiveMemberEvictedBehindItsFollowUp`.
- Green on dev, the classification: `shard.zig` "THE DOUBLE PARK … at the membership core"; the model
  `park_core_model.zig` invariant 3 (`ReArmModel`): its weakening, the release before the learn, fails —
  a committer reads RUNNING and the stale deadline and re-parks a member just granted.

## Counts (on `4d9ae87`, beside the full smoke looped through `scripts/venue`)

- Full smoke 22 runs: 19 green; the three reds are single-daemon rows (THE INTERCEPTION HOST, `umount
  --lazy` of a BUSY mount, the venue-exclusive FROZEN DAEMON beside other mounts). The five membership
  rows (this one, F4, DEAD MANAGER, KILL ROWS (b), SHARED HOT at 3 s) passed in all 22.
- Red unit set 100/100; `membership` 100/100 (102 tests), `reclaim` 100/100 (102), `lattice` 100/100 (58).
- This row + DEAD MANAGER + F4 20/20 (24 renewals through each 7.7 s hold); KILL ROWS a/b/c + SHARED HOT
  10/10; the recovery/kill/membership batch (21 rows) 67/67 tests; rails 32/32; `zig fmt --check` clean.
- Before the rebase, the same sets 100/100, 20/20, 7/7, 67/67; 8 of 10 loads green (one red was KILL
  ROWS (b)'s race, closed by `4d9ae87`).

## Seen, not fixed

- The `-o ro` reader's beat re-joins and re-points inline after a lapse (a lapse costs a re-join, not a
  poison).
- The re-point after a reclaim follows the gate's release (on dev too): between them a read can be served
  from a token of the dead manager's grant table; the old comment's "before the held acks drain" never held.
- Beside the full smoke, outside membership: THE KILL MATRIX `staged_payload_lost_reads = 4` and THE
  SIDEBAND `deferred >= 1` (both also red on the c3 agent's tree: `~/tmp/c3/load-4.log`, `load-10.log`),
  ATIME `ScriptFailed`, THE INTERCEPTION HOST (`e2e_linux.zig:425`), `umount --lazy` of a BUSY mount. A
  dead `--supervise` mount (`/build/tmp/mnt-4176279`) held host-wide `sync`; unmounted by hand.
