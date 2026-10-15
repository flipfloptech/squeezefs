# Rung 10 — the transient fold past the ONE save's size (epochcap)

The two leftovers `rung-10-silentdrop.md` noted on `src/data/write_epoch.zig`, plus pins for two
of its untested best-effort drops.

**1. The fold crashed the daemon (release blocker) — reachable.** `close` moves the open epoch E
to `closing` (`write_epoch.zig:397` on `f0f5ee4`) and runs the ONE save outside the spin lock
(`:503`). A record landing in that window opens the next epoch E2, and `record` capped only E2's
own count (`:288`), so E2 can take 64 blocks E does not shadow (generic/247's shape: the epochs
fill to the cap back to back). When the save answers `Transient`/`OutOfMemory` (`:528`;
`plane.mapMerge` returns them on a layout read/decode failure `plane.zig:2439/2444`, a refused
publish or commit `:2482/:2496`, a foreign ship that did not land `:2520/:2532`, a refused
`backend.begin` `:2475`), `reregister` folds E's shadow into E2 (`:467`): up to 128 blocks. E2's
close then hit `assert(n < inserts_buf.len)` (`:492`) — a panic under ReleaseSafe.
**Fix:** `record` caps the open shadow UNITED with the closing one's (`shadowUnion`, ≤ 64
lookups); a block past it answers `save_in_flight` (the callers already await the save and record
again), with no save in flight `not_shadowed`; a block the closing epoch shadows never grows the
union, so it is always taken — the generic/247 visibility law now holds by construction. Every
epoch, a fold's included, stays ≤ `max_displaced`; no allocation was added under the lock. The
assert is the must-stay-0 tripwire `rewrite_shadow_save_overflow` (stats JSON): it re-registers the
epoch whole (nothing published, nothing freed).

**2. `record` was not all or nothing.** A refused `create` left an undefined epoch pointer in the
table (`getOrPutAssumeCapacity` came first, `:298-300`); a refused list/shadow/guards reservation
left an empty epoch registered with no room for a fold (the next transient fold would overrun its
assume-capacity puts). **Fix:** reserve the table slot, the epoch and its three reservations
first, register last (`errdefer` destroys a fresh epoch).

**3/4. Pins.** `manager.appenderOfIdentity`: the identity insert refused ⇒
`manager_identity_cache_drops` +1, answer still the walk's (1), cache empty, the next miss
walks and caches. `SlotHolderCache.forget`: the hint refused ⇒ `slot_resolve_hint_drops` +1,
the learnt holder forgotten, `stepHome` local, `knows` false (the manager is asked again).
Not done (budget): `dir_stripe_cache_drops`, `fuse_parent_memo_drops`,
`block_grant_declarations_uncounted`. Their sites need a whole-harness allocator, where the
failing index is hard to pin.

| test | red (`6dcb183`, macOS aarch64) | green |
|------|------|-------|
| "a transient save's fold never carries more than the ONE save's size …" | `expected 64, found 96` (`:1282`) | ✓ |
| "record is all or nothing …" | `expected 0, found 1` (`:1320`) | ✓ |
| "a shadow past the ONE save's size … is loud and never lossy" | — (the fix's arm) | ✓ |
| generic/247 test, rewritten for the union | — | ✓ |
| manager identity / resolve hint pins | pass (they pin today's behaviour) | ✓ |

**Verified.** Locally: `write_epoch sym.manager slots.resolve`, 71/71 in 7 fresh runs.
Kamrui (x86_64), the full gate (`-Dfresh=true`) over `631f83f`+`3b1c199` (then on `f0f5ee4`; rebased since over a docs-only commit): `53/53 steps
succeeded; 2979/3062 tests passed (83 skipped)` (the five new tests over leak67's 2974).
