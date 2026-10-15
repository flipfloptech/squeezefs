# Rung 10 — allocation failures that dropped state silently (silentdrop)

Same family as `rung-10-epochleak.md`: product code swallowed an allocation failure with
`catch {}`, so an OOM dropped a block, a guard, a key or a queue entry without a word. The
24 single-line sites (an allocating call ending in `catch {}`, outside `src/cli` and tests)
plus their neighbours of the same shape (`catch return` on the same state, the screen's
`heldKeys(…) catch {}`), classified: **a** drops state that matters → fixed (reserve before
the transition, or no allocation at all); **b** a best-effort cache/hint → counted, with its
fallback on the counter's doc line; **c** a RAM test double → fails loud (`@panic`).

| site | class | fix | test |
|------|-------|-----|------|
| `write_epoch` `reregister` fold: shadow put, displaced/guards appendSlice (+ the table `getOrPut` arm that orphaned the epoch) | a | `record` keeps a table slot free per closing epoch and, under a closing epoch, room for its shadows/parked keys/guards; the fold uses assume-capacity puts | "a transient save folds into the next epoch with no allocation …" |
| `write_epoch` `closeEpoch` drift arm append | a (tripwire arm) | drift keys go to a fixed `[max_displaced]` buffer and are freed with the parked set | "a drift key the save displaced is freed with no allocation …" |
| `shard.rememberDepartedKey` | a | the join reserves a departure slot per member; the append assumes capacity | "an evicted member's key stays held through an allocation failure …" |
| `death_ledger.screenDeath` `heldKeys catch {}` (screen failed open) | a | `Owner.holdsKey` (no allocation); `heldKeys` deleted | same test |
| `kv_fs_stripes.install`: stripe_of puts, migrating append (+ maps `catch return`) | a | all or nothing: the three reserved first; a refusal caches nothing, so the next verdict re-reads and queues the migration | "a verdict install is all or nothing …" |
| `zcrx/lane` `feedZcrx` deferred append (+ `refillPass`'s) | a | `deferred` reserved to one slot per chunk at init (a slot is deferred at most once) | lane ring test: `deferred.capacity >= records.len` |
| `kv_fs_stripes` supplyFn / verdict stripe_of puts, plain put (×3) | b | `dir_stripe_cache_drops` | — |
| `kv_fs.rememberParent` (`..` memo) | b | `fuse_parent_memo_drops` (`..` ENOENT until the dir's next lookup) | — |
| `resolve.forget` known_unleased | b | `slot_resolve_hint_drops` (one more manager ask) | — |
| `manager.rememberIdentity` | b | `manager_identity_cache_drops`; the miss answers from the directory walk, not the cache | — |
| `plane_arm.declareWindows` declared | b | `block_grant_declarations_uncounted` (the report waits a renewal) | — |
| lease `RamSink` ×5 (+ dupe `catch return`), recovery `RamVolume` ×2, conveyor `RamLane`, migrate storm | c | `@panic` naming the double | — |

**Red → green (kamrui, x86_64).** On the red commit: fold — a recorded B unpublished
(`write_epoch.zig:1060` expect fails); drift — `expected 1, found 0` freed; stripes —
`expected 1, found 0` queued migrations; screen — the death naming the departed key accepted;
lane — `deferred.capacity >= records.len` false. All green on the fixes; the filters
`write_epoch kv_fs membership slots.resolve sym.manager plane_arm zcrx.lane jobs. conveyor
routing.migrate write_admit device_overlay fsync`: 356 pass / 16 skip.

**Not fixed here (noted).** A transient fold can carry the union of two epochs' shadows past
`max_displaced` (`record` caps only the open epoch), which `closeEpoch`'s `assert(n <
inserts_buf.len)` would then trip; and `record` itself is not atomic on a mid-way allocation
failure (a fresh epoch can stay empty). Both pre-date this change.

**Full gate** (kamrui, `-Dfresh=true`, 1,157 s wall): `53/53 steps succeeded; 2970/3053 tests passed (83 skipped)` (the four new tests over the epochleak gate's 2966).
