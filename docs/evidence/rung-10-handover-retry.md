# Rung 10 — the slot-move retry law (the last release-gate blocker)

Decision 0003 §X (the law, §X.1–§X.2, and the six defects under it, §X.3–§X.8), 0002 row 517.

## 1. The defect

An op shipped across the wire, or one step of a cross-owner change, that met a slot
WHILE it moved got a retryable refusal (EAGAIN / `HolderUnreachable` / Busy), and the
refusal reached the user. Measured before this work (the gate agent's attribution,
`rung-09-gates.md` §10):

| Row | Symptom | Rate |
|---|---|---|
| `smoke: THE HANDOVER DRAINS PARKED STAMPS` | B's `chmod` → EAGAIN | failed 12/20 on `53b9a9b`, 34/40 on `da080b7`, ~40 % of full gates |
| `smoke: ONLINE FSCK THROUGH THE ADMIN LANE` | B's `printf > xN` into A's directory → `File exists` or EAGAIN; offline fsck `[C9] … live, prior-era, named by no dentry`; once `scrub --online` → `HolderUnreachable, AGAIN` | failed 9/12 on `39590f0` and `2e071ea` |

## 2. The law (0003 §X.1–§X.2)

Every retryable refusal a cross-owner op or a served/shipped verb receives during a slot
move (a handover, recovery's coherence hold, an eviction) is retried by the SYSTEM:

- **The gate.** `src/fuse/handler/move_gate.zig` wraps the served `Filesystem` — the FUSE
  dispatcher's and the IPC host's (`src/cli/mount.zig`). An attempt that answers a move class
  (`errors.isMoveClass`) parks on the mount's slot-move epoch (`src/meta/sym/slots/moves.zig`,
  bumped by every lease-table and resolver transition through `LeaseTable.noteMoved`), then
  runs again from the top with every home re-resolved. A remote move wakes nothing local, so
  the park re-polls, doubling 1 ms → 40 ms (one device barrier → twice the measured upper
  handover).
- **The bound.** Past `T_park_max` (`park_reclaim.tParkMaxMs` at the membership's `T_owner`,
  `WirePlane.moveDeadlineMs`) the op answers the TERMINAL `HolderUnresolved` (EIO), loud; the
  mount's close ends every park with the same answer. Never an unbounded spin, never EAGAIN.
- **Idempotent re-issue.** An intent a move left open stays the live op's
  (`xv/ops.zig` `executeHeld` → `HeldOpen` on `Ctx.move`); the re-issue resumes it by its id
  and answers the first attempt's result (a create's child ino — never EEXIST against itself;
  a remove / rename / link / write postlude their landed state). An op that ends otherwise
  abandons the intent to the roll-forward, which completes or compensates it (§Q.19, §U.4).
- **Observability.** `slot_move_reissues`, `slot_move_parks`, `slot_move_park_wakes`,
  `slot_move_repolls`, `slot_move_unresolved`, the `slot_move_ns` phase; `xv_held_open`,
  `xv_resumed`, `xv_resumed_completed`, `xv_resumed_gone`. A move class reaching a FUSE reply
  fires the `fuse_move_class_at_boundary` tripwire.
- **Model.** `src/meta/sym/slots/move_epoch_model.zig`: observe → attempt → register →
  recheck → park never loses a transition's wake. Weakenings that fail: `recheck_before_register`
  (the recheck before the registration loses a bump between them) and `observe_after_attempt`
  (sampling the epoch after the attempt sleeps through the move that refused it).

## 3. What the rows found under the law

The gate alone turned EAGAIN into parks; the re-issues then exposed what a moving slot
showed them. Each defect below landed with its pin.

| # | Symptom under load | Cause | Fix (0003) | Pin |
|---|---|---|---|---|
| 1 | B's committer spun on refused first touches until the grant | The door sent a committer to `first_touch` on a slot the stand reserved for another member | The door refuses naming the accepter (§X.3) | acquire "the door in a handover's release → grant gap" |
| 2 | B re-asked AcquireSlot for its own slot until the accept's install, forever when the reply was lost | A replayed AcquireSlot answered `already` with no words | Replayed items carry the grant's words; the joiner's resolver settles its own grant (§X.4) | acquire "an AcquireSlot REPLAYED by the slot's holder" |
| 3 | B's create into the moved directory: `dying parent` → EEXIST, ENOENT; an empty tree shadowing the transferred root | The install published the table word before the tree; a commit in the gap minted an empty tree | Tree first, word last; no mint for a slot this mount may not write; the shadow tripwire (§X.5) | handover_moves "THE ACCEPTER'S INSTALL OF A WIRE GRANT" (`park_mid_install`) |
| 4 | `[C9]`: every name B created in A's directory gone after B's unmount (A listed `x44`/`x45`, the device held `x0`–`x44`) | A wire lessee appends into leaves IN PLACE under an unchanged root; A re-adopted the released slot off its pre-grant cached leaf | The cache barrier: every lease move across processes drops the slot's node mappings (§X.5) | handover_moves "THE CACHE BARRIER AT A LEASE MOVE" — red `TestRecordLost`, green after |
| 5 | `scrub --online` → `HolderUnreachable, AGAIN` | A's daemon panicked: the online scope's window scan asserted on an intent home (local 0) from B's ring window | Inode keys only (§X.6) | census "an inode-owned key ino has a global ino; a slot's reserved locals … do not" (the venue's panic trace is the red) |
| 6 | B's rename inside A's directory → ENOENT, with or without a handover | An op whose every object is one foreign holder's ran as a local commit and revalidated against this mount's view | `kv_fs.needsIntent` (§X.7) | handover_moves "THE MOVE MATRIX" — red at the offered and drained steps (`da4004e`), green with `aa0acb1` |
| 7 | The first full gate on the law (`df5549a`, the pre-rebase `aa0acb1`; green): in the shared hot directory row C's create into a stripe of the killed B parked 48 s, answered EIO (`move gate: … did not resolve in 48000 ms (last answer StaleHolderView)`), C created 5 files across the death (thousands in every earlier gate), and C fired `xv_cross_owner_intent_stuck` | The re-issue re-took the plan's guards at the dead holder each time: §Q.16 gave a step's ship the re-resolve, never the guards | `acquirePlanGuards` re-resolves a dead or stale view before giving up (§X.8) | two_holders "the plan's guards meeting a DEAD view of a key's holder re-resolve the slot" — red `49fa753`, green `86a1389`; the row 6/6 after (C 5,743–6,659 creates across the death, no move-gate EIO, no tripwire) |

## 4. The red pins per handover step × op

`src/meta/sym/handover_moves.zig` "THE MOVE MATRIX" (Linux; two real members in one process:
A the manager with its wire plane over a file-backed volume, B joined over TCP; both
filesystems bound the way `mount` binds them; B's behind the product's `MoveGate`). A's
directory in one of A's rotor slots moves to B by a holder-decided handover held at a step
while B issues one op into it — one pair of members per op, 20 fixtures:

| Step (the seam) | create | chmod (record ship) | unlink | rename | setattr (record ship) |
|---|---|---|---|---|---|
| offer pushed, stand raised (A still holds) | green | green | green | red → green (#6) | green |
| step 0 drained, A's door closed (`park_after_page`: past the drain, the gate, the flush and the page) | green | green | green | red → green (#6) | green |
| grant landed, B's install mid-way (`park_mid_install`) | green | green | green | green | green |
| door re-opened (the move done) | green | green | green | green | green |

Each op must answer one successful call's result (no error at all: no EAGAIN, no EEXIST, no
EIO), the slot must be B's on both tables, and BOTH members must then read what one call
leaves (the created name at the returned ino; the unlinked name gone with nlink 0; the
renamed name at the source's ino; the mode; the times). The held steps release only once the
op is in the window — finished, parked at A's door (`slot_door_parks`), parked on B's epoch
(`slot_move_parks`) or waiting behind B's held install (`slot_install_waits`) — bounded at
30 s, a miss failing the pin. The step-0 drain itself runs before the door's gate, so an op
meeting it sees the offered state; the `park_after_page` seam holds the first point at which
A's door is closed. The online-fsck job-table read is the coordinator's own table (volume 0's
manager), never slot-routed; its failure (#5) was the daemon's panic and is pinned by the
census row and the smoke row.

## 5. The two rows, 40 consecutive each, under 8-way load

The landed tree (`d73d7a5`, on `dev` `9748bb1`), eight `yes` spinners beside each run, each
loop touching only its own smoke process's work dir:

| Row | Runs | Green | Load during the loop |
|---|---|---|---|
| `THE HANDOVER DRAINS PARKED STAMPS` | 40 | 40 | 1.5 at the first run, 8–10.4 after |
| `ONLINE FSCK THROUGH THE ADMIN LANE` | 40 | 40 | 4.5 at the first run, 8–9.3 after |

The same 40/40 held on every earlier tree of this branch: `41bbf03` (before the first rebase),
`df5549a` and `ee93c60` (pre-rebase hashes of `aa0acb1` and `86a1389`; loads 8–11). The handover row failed 34 of 40 before this work.

## 6. Gates

| Gate | Tree | Load | Result |
|---|---|---|---|
| macOS `zig build gate` | `d73d7a5` (landed) | — | 28/28 steps; 2,206 of 2,233 passed, 27 skipped |
| venue R1 (fresh `--cache-dir`) | `d73d7a5` | 0.1–0.9 | 46/46 steps; 2,803 of 2,875 passed, 72 skipped (unit 2,564 + 69 skipped, smoke 86 + 3 skipped, nvmet 53, rails 16) |
| venue R2 (fresh `--cache-dir`) | `d73d7a5` | 0.1–0.9 | 46/46 steps; 2,803 of 2,875 passed, 72 skipped |
| venue R3 (fresh `--cache-dir`) | `d73d7a5` | 0.1–0.8 | 46/46 steps; 2,803 of 2,875 passed, 72 skipped |

Earlier, on the branch before its last rebase: gate 1 on `df5549a` was green but its log
showed defect 7 (§3) — C's 48 s park and EIO, C's `xv_cross_owner_intent_stuck` — fixed in
`86a1389` (`ee93c60` before the rebase); gates 2–4 on `ee93c60` green (46/46; gates 3 and 4 reused gate 2's cached unit run,
hence the fresh cache directories for R1–R3).

Every gate's log carries two non-expected tripwire lines, both from `smoke: THE FLEET'S MERGED
BLOCK CENSUS` with its `SQUEEZEFS_TEST_SHIP_EVERY_FREE` seam ON (the row injects the defect
and asserts fsck reports it): `fsck_findings` and `block_untracked_free_refusals`. The same row
on pristine `dev` `7adc3ec` logs the same two. The four `move gate:` warnings per gate are the
unit tests' own (a 3 ms deadline; `kv_fs` "the parked joiner's `.stats`" and `move_gate`).

## 7. Not done, stated where it stops

- The matrix holds the handover at four points; a seam that parks INSIDE step 0's drain was
  not added (the drain runs with A's door open, so an op meeting it takes the offered row's
  path). The resume of an open intent has a dedicated pin for the create only (two_holders
  "a create whose insert met a slot MID-MOVE stays the LIVE op's"); the remove / rename / link
  / write-postlude resumes are exercised only where a matrix op happens to leave its intent
  open, with no row of their own.
- The red evidence for defects 1–5 is the working tree's red runs during this session
  (the pins were written against the failing behaviour, then the fix); #6 and #7 landed as
  separate red commits.
