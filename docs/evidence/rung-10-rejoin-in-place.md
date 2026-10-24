# Rung 10 — a live member evicted re-joins in place when it holds nothing un-landed (2026-10-24)

The owner's ruling (b): a joined mount its home shard evicted at `T_owner` (a partition, a frozen
daemon) re-joins IN PLACE — a fresh join under the same live FUSE mount — when it holds no un-landed
state; otherwise it fail-stops as before. Charter §5.5 (amended), decision 0003 §BS, 0002 row 558.
Venue: this workstation, `7.3.0-1.7-mainline-sqz`; branch `feat/rejoin-in-place` off dev `885727b`.

## The law

**Eligibility — "nothing un-landed", judged on the RETIRED incarnation** (`WirePlane.judgeUnlanded`,
`RejoinHost.unlanded`): every window it admitted landed and no reservation is open; its ring's landed
head equals the head the successor's recovery read (its own page `Recovered{ledger_tail_seq}` off the
device, polled within `T_park_max`); no cross-owner intent open; no data block in the writer's in-flight
registry; no staged write-cache resident; no write-side custody (parked block, open overlay, open or
saving rewrite epoch); no packed tenant between reserve and commit; no parked time refinement. Any
word set ⇒ the park's expiry, the poison, EIO until remount; the refusal names the word.

**The sequence** (on the beat that met `UnknownLease`; `shard.Member.tick` answers `evicted`):
1. the follower ends; the writer's block-grant windows are forgotten (never returned);
2. every volume RETIRES (`JoinedSet.retireIncarnation`): the latch refuses new mutations `Retired`
   (the conveyor's `Deferred`), the park gate takes RETIRED (one CAS off RUNNING/PARKED), the growth
   gate closes and drains — passes, then windows — within one landing ceiling, then the latch fails
   under the SMO lock: no cycle, page or landing writes again;
3. the eligibility (above);
4. every volume joins afresh through the mount's own door (`KvBackend.openJoined`: census, endpoint
   poll, `awaitManagerListener`, `JoinAppender`, rotor, first checkpoint); its projection's rows naming
   the predecessor load unleased;
5. every token dropped; the successors published (bindings re-applied, one pointer store per member,
   the retired incarnation kept mapped until the unmount); the member joins afresh (new epoch, its
   listener re-published under the new appender id); the read cache and the kernel's whole view drop;
6. the move epoch bumps.

**Bounds and in-flight ops:** the drain ≤ one landing ceiling (1.1 s); the recovery wait and the
membership join ≤ `T_park_max`; measured end to end 160–200 ms after the beat's release at the smoke's
`T_owner` of 3 s. A read in flight completes off the retired RAM (the recovered state); a commit
answers `Deferred` and the move gate re-issues it on the successor (≤ `T_park_max`); only a refused
re-join answers EIO (the poison closes the move epoch at once).

## The lock-free core and its model

`park_core_model.zig`: invariant 2 now races release, expiry AND retirement off PARKED — holds under
the CAS, the load-then-store weakening lets two act; invariant 4 (`RetireModel`) — a pass enters the
growth gate (an RMW), reads the word, tracks its window, leaves; the retirer CASes RETIRED, waits the
passes, then the windows, judges. Holds; **weakenings that fail:** the pass's fence removed (its
increment buffered while it reads RUNNING), and the window count read before the pass count.

## Defects found on the way (fixed here)

- **The successor's stale projection.** Its checkpoint can predate the recovery's tree-0 release, so
  slots it names the PREDECESSOR the lessee of were foreign holders no one is: a shipped write publish
  to A met its coherence hold for every retry. `JoinOptions.predecessor` → `joinedProjectionRowOf`.
- **The owner's fresh join (`graceExpects`).** A predecessor-roster member stayed "expected" for the
  owner's life: after it re-asserted and was evicted (or left, or remounted), every join without an
  epoch took the reclaim path and answered `UnknownLease` — the re-join's membership join under a
  remounted manager was refused (the dead-manager eviction row). `graceAwaitsReassertion`.
- **The double join of the follower** on a refused re-join (SIGSEGV at umount): `follower: ?Thread`.

## Red tests

| Test | dev `885727b` | branch |
|---|---|---|
| smoke "A LIVE MEMBER EVICTED WITH NOTHING UN-LANDED RE-JOINS IN PLACE" (idle-clean B, a file held open read-write, beat held past `T_owner`) | `PoisonedInsteadOfRejoined` | re-joined; the held fd writes on the successor; A and B byte-equal; create + rename in A's dir; exit 0 both; fsck 0 findings |
| smoke "… WITH UN-LANDED STATE FAIL-STOPS" (64 KiB held in custody, `SQUEEZEFS_WRITEBACK_POSTURE=close`) | poisoned, no judge | "cannot re-join in place — write-side custody on 1 ino(s)", EIO in 2 ms, exit 1 (crash shape), fsck clean |
| unit `park_core_model` invariants 2 (three exits) and 4 (`RetireModel`) | — | hold; the weakenings fail |
| unit "THE RETIREMENT …" (park gate), "the retired latch …", "THE RE-JOIN IN PLACE's verdict …" (`Member`) | — | green |
| unit "a predecessor-roster member that re-asserted, then left or was evicted, joins FRESH …" | `UnknownLease` at both arms | green |

The eviction row under the dead manager's striped directory (F4) now holds custody at the eviction:
its verdict is the fail-stop deterministically (its storm alone left B eligible in 1 run of 3).

## Counts (on `8dba8fa`, before the rebase)

- The three eviction rows (the re-join, its negative arm, the dead-manager eviction row) **20/20**,
  two copies in parallel × 10 rounds beside the membership batch loop (and a full gate in the main
  tree): re-joined 121–202 ms after the beat's release, A evicting B 4.5–4.9 s into the hold.
- The membership/recovery/kill batch (26 rows: N-DAEMON, the clean rejoin, the three manager-remount
  joins, TWO MOUNTS OF ONE SET, KILL ROWS a/b/c, KVDA ×2, SHARED HOT, DEAD MANAGER, the four
  live-member rows, the `-o ro` reader, the ordered law's kill row, ONLINE FSCK) **4/4** runs green
  (+ 2 single runs before).
- Unit filters `membership`, `reclaim`, `lattice`, `joined` (+ `RE-JOIN`, `retire`): 249 pass, 24
  skipped, **20/20** runs; `journal`, `conveyor`, `lease_table` with them 433/462 (29 skipped).
- `zig build rails` 32/32; `zig fmt --check` clean.
- Rebased onto dev `eeec3e8` (the data-plane answers, §BO–§BQ): the three eviction rows 11/11
  (2 × 5 + 1), unit filters with `journal`/`conveyor`/`lease_table` 442/472 (30 skipped), rails 32/32.

## Seen, not fixed

- **A joiner's APPEND to a foreign-slot file reads back zeros on both mounts** — dev, no eviction:
  A writes `own/f` (1 MiB), B appends (`echo x >> own/f`, or through a held `exec 3>>`): size right,
  appended bytes zero on A and B (B's stats: `drain_inplace_write_bytes` 4096, `record_ships` 1;
  A's: `layout_map_merge_failures`, `slot_door_coherence_refusals`). Repro in
  `~/tmp/rejoin/smoke-with-control.zig` (TEMPCONTROL). The re-join row overwrites through its held fd
  instead. Owed to the data plane's owner.
- The retired incarnation stays mapped until the unmount (its node cache is budget-counted memory).
- A refused membership join after the successor was published poisons the mount; the successor's
  dismount runs its leave against FAILED latches — best effort (the next open's recovery ladder).
- Between the eviction and the beat's verdict the evicted member serves reads off its stale RAM
  (bounded by `T_owner − T_self` plus the hold); B's checkpoint task may still write its page in that
  window on a non-PR substrate (a zombie page write would read Live and refuse the re-join: safe).
