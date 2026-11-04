# Rung 10 — the membership owner's one lock (decision 0003 §DM)

§DL.4 found the membership owner (`shard.Owner`) served unlocked; the independent safety review
confirmed it blocker-class.

The serving threads are one OS thread per connection, plus that connection's serve lanes. The
membership cadence also ticks the owner. Only joins, leaves, evictions and `writerKeys` took §DL's
roster `SpinLock`. Everything else read or wrote the roster unlocked: `renew`, `census`, the
`RecordDeath` screen, `liveWritersFn`, the token lease oracle and `expireDue`'s scan.

The worst outcome was a lost renewal:

1. A join's append reallocated the roster under a renewal's stores.
2. The renewal landed in the freed buffer.
3. The live member was evicted at its old deadline.
4. Its host was fenced, and the member fail-stopped with its unsynced data lost.

## What changed

- **One lock.** `Owner.lock` is a `sync.Mutex` (charter §9's async mutex over the `Io` futex); the
  owner now carries its `Io`. The shared state lives in `Owner.owner_roster`, which only
  `lockRoster` hands out. It covers `members`, the epoch and join counters, the grace's progress,
  the departed keys, the offer and release carriage, and the renewal's screen bounds and sink.
- **RAM work only under the lock.** Every call out runs with the lock released: row writes, the
  death fan-out, window declarations and callbacks. Holding no other lock while held, it is the
  innermost lock on every thread (§DM.3).
- **Join.** Applied whole under one hold, with every allocation first. The re-assertion is noted
  together with the append.
- **Eviction.** Each due member is judged and removed under one hold, so a racing renewal either
  lands first or answers `UnknownLease`.
- **The death screen.** One read: `Owner.deathScreen`.
- **No pointer into the roster escapes.** Callers get copies or counts.
- **Row deletes.** A departure's row delete re-writes the row of an incarnation that joined again
  meanwhile.
- **The static rail.** The owner lock rail names `owner_roster` 4 times in `shard.zig` and nowhere
  else.

## The reds

The seams commit is `eacb370`: `Owner.test_hook`, plus `shard.Race`, which parks a caller at a site
until the racer finishes or waits on the lock. No sleep is involved. The reds are committed alone
in `b837101`, and each failed 3 of 3 runs on the seams commit:

| Red | On `eacb370` |
|---|---|
| A renew parked between its lookup and its stores; 64 joins reallocate the roster (`DebugAllocator`, never_unmap) | `shard.zig:2143` `expected 0, found 1`: the renewed member was evicted at its join's deadline |
| `expireDue` parked between judging a member due and evicting it; the member's renewal (its clock just inside the deadline) arrives | `shard.zig:2170`: the renewal was granted AND the member evicted |
| The screen's held-key scan parked mid-roster; a leave swaps the key's live holder behind it and a join fills the tail | `death_ledger.zig:720`: the death was accepted |

The fix is `8e2b892`. The churn unit `1236b14` runs steady and flaky members, joiners and leavers,
the carriage, every reader face and the cadence, each on its own thread, with the clock in lockstep
with every participant.

## The numbers

All runs on kernel.org 7.3.1 (Omarchy, multipath=N), on the final code.

| Check | Result |
|---|---|
| The three reds, ×10 uncached | green |
| The membership, wire plane, guard gate, census and token units | 312 passed / 26 skipped / 0 failed |
| The 27 eviction / re-join / KILL rows, ×3 | 71/71 each |
| The nvmet tier, ×1 | 70 passed / 1 skipped (multipath-tcp under multipath=N) / 0 failed |
| The full smoke | 186 / 199 (13 skipped) |
| FULL `zig build test` | 3,317 / 3,510 passed (193 skipped), 0 failed |
| Rails | 31/31 |
| `zig fmt --check` | clean |

The loaded churn ran for 775 s beside nproc/2 busy loops and the full smoke's 16 shards in passes
(load 22–54):

| Counter (20 churn runs) | Value |
|---|---|
| Joins | 339,536 |
| Renewals | 2,909,584 |
| Evictions | 2,642, every one a flaky member's deliberate lapse |
| Steady members evicted | 0 |
| Wrongful evictions (latest grant younger than `T_owner` at the eviction's clock) | 0 |

Beside them, four runs of the multi-daemon membership rows (N-daemon posture, live member evicted,
re-join in place, clean unmount and rejoin, the KILL rows) each passed 55/55.

## Limits (§DM.8)

- Two incarnations' rows land in completion order.
- A re-written row can race a later leave.
- `census` pages past 1,025 members can skip members (pre-existing, found).
- The renewal's probe stays linear.

Not run: the full gate and any timing benchmark; both are the orchestrator's.
