# Rung 6 wave C1 — cross-owner intents, the record-level ship, directory striping (evidence)

Charter §5.5 "Cross-owner namespace ops", "Record-level mutations of a foreign-slot file",
"Directory striping"; §5.2 (rename = ONE entry locally); §2 lock order; spec
`docs/porting/sym_tokens_intents_striping.md` §2–§4, §5–§8; decisions 0002 rows 272–279,
0003 §E (the rung-5 `KvTx.cross` / `commitSplit` / EXDEV stopgap retired).

## What landed

- `src/meta/sym/xv/intent.zig` — the v4 record: `IntentHeaderWire` (32 B, `"SQZXTENT"`, xxh3
  over the field-zeroed image) + `XvStepWire` steps (48 B + the padded name; every unused word
  checked zero per kind), `XV_MAX_STEPS = 2 × MINT_SPREAD + 8 = 136` (the one-intent `rmdir`
  of a K = 64 directory), the KEY law (`(home, tx_id & HASH56_MAX, tx_id >> 56)` — the id IS the
  key, injectively), the HOME law (local 0 of a slot's namespace, which no inode owns), the
  stored `xtx:{tx_id:016x}` envelope, the ABORTING flag (row 272), the `Minter`
  (`term << 40 | seq`, recovered as `max(open) + 1`, row 273), the `Register`
  (`minted ≡ retired + open`; `stuck` past `T_owner` is the must-stay-0 tripwire).
- `src/meta/sym/xv/steps.zig` — the six kinds with their witnesses, the inverses, the pure
  per-op plans (`create` = `[CreateInode @ creator's rotor, InsertDentry]`; `unlink/rmdir` =
  the name FIRST then the count (no count for a dangling name); `link` = the count FIRST
  (EMLINK before any step); `rename` = `[dest count, dest remove]? + remove + insert +
  TouchCtime + whiteout`; `exchange`; the flip; the one-intent striped `rmdir`).
- `src/meta/sym/xv/applier.zig` — THE ONE APPLIER, generic over the `Store` contract: one
  whole-tx entry per step with the intent RIDER beside it (committed even when the witness
  declines), idempotent under the witness (apply twice ⇒ once, every kind), the dying-parent
  refusal, `destroyUnnamed`, the home scan.
- `src/meta/sym/xv/guards.zig` — travelling 4a guards: `ParkedScopes` (served `XvGuards` →
  the holder's canonical `lockManyWide` parked under the scope; a second ask skips covered
  stripes; `XvRelease`; the expiry sweep is the belt — must-stay-0), the `Covered | Take`
  verdict by `(class, stripe)` through `dlm.Manager.covers`, `acquire` across tables in
  ascending appender-id order (the local table at its rank; one `XvGuards` per remote table).
- `src/meta/sym/xv/dir_rename.zig` — the set-wide lease over `control_records.DirRenameWire`
  (`Lock → Locked{already} | Busy{holder}`; `Unlock` by the holder; the screen: volume 0, a
  non-own id, an unlock naming the holder; a dead holder's release), the `Taker` (one permit
  per identity + the park on `Busy`, never a refusal), the exact ancestry walk (memo link →
  scan belt → `LookupExact` at the parent's holder; ≤ 3 rescans ⇒ EAGAIN; > 4096 hops ⇒ ELOOP;
  a stripe continues to its directory).
- `src/meta/sym/xv/ops.zig` — the executor: step 0 + the intent (riding step 0's entry when
  local, else written alone into the own rotor slot), the barrier, dispatch classified by the
  ARM TAKEN (row 274), `SlotMoved` re-resolve + re-dispatch ≤ 2 (the late try-only guards for a
  slot that moved TO the initiator), the live-refusal compensation (flip ABORTING → inverses in
  reverse → retire), a failed ship leaves the intent OPEN (abandoned), the synchronous
  retirement; `recoverOpen` at mount + `cadenceTick` (adoption by register STATE; the inverse
  plan under ABORTING; reconciliation against the durable scan); the served side (`serveStep`:
  the lease check FIRST as the block's not-holder word, the child screen (tripwire), the guard
  verdict, the apply; `lookupExact`); `StepService` on the wire. Phases
  `plan + intent_barrier + Σ ship_rtt + local_steps + retire ≡ total` from chained instants.
- `src/meta/sym/xv/record_ship.zig` — `setattr/setxattr/removexattr/publish_layout` as ONE
  verb each; the resolver; the ship with one redirect; the served side under the holder's
  door; `ServedSink` (attr cache whole, layout entry under the dirty law — `DirtyKept` beside a
  publish is the tripwire — then `INVAL_INODE` attrs-only / whole, then `PRUNE` iff
  writeback ∧ uapi ≥ 7.45, caps passed at install — row 275/Q16).
- `src/meta/sym/xv/stripe.zig` — markers (two digits exactly), `decodeMap` (live iff the
  commit marker names stripe 0 over a contiguous index set, no self-reference), `stripeOf`,
  the plans, `foldAttrs`, `nFloor`, `DirCensus` over the half-window pair + the bounded
  `Census` (`CENSUS_DIRS_MAX = W / 64`, single-flight), `pageFromStore`/`readMap`/`locate`,
  the streamed K-way `readdirMerged` (markers filtered, D's stream the LWW loser), fsck C17
  (five shapes, report-only, `fsck_stripe_findings`), `Striping(Store)` — the four-phase flip as
  ONE intent (`lockManyWide` once, after the suppliers' asks and before the remainder's mints),
  `mutationParent` (lazy re-home), `migrate`, `statFold` (persisted once per cadence),
  `rmdir` (ONE travelling exclusive set over `D` and every stripe, row 276).
- `src/meta/sym/xv/set_bind.zig` (Linux) — `Plane(MetaSet)` over the set: every slot's home is
  its volume (a flat multi-volume set spans volumes with every slot local — the intent IS the
  cross-member transaction); a slot another appender leases resolves `unreachable_` until the
  wire join lands.
- Bindings: `kv_fs` — `bindXv` (the roll-forward before serving), `spansHomes` at
  create/unlink/rmdir/link/rename (+ `redispatches` once on a door `SlotMoved`), the
  `xvCreate/xvRemove/xvLink/xvRename` arms (a directory source takes the lease + the ancestry
  walk), `foreignSlot` → the record ship at setattr/setxattr/removexattr, the destroy batch cut
  per member, the marker filter in `listDir`, `user.squeezefs.stripes` read as the width in
  force; `cli/mount.zig` + `cli/offline.zig` bind the plane (the cadence rides the reclaim
  fiber); `meta_set.commit` refuses a split on the tripwire `routing_split_refused`;
  `tx.CrossVolume`, `commitSplit`, `routing_split_*`, `error.CrossVolume` deleted;
  `dlm.wide_requests`/`lockManyWide`/`covers`; `lock_order.max_held = 512`.
- Knobs: `SQUEEZEFS_SYM_DIR_STRIPES` (A/B lever, 1 = off), `SQUEEZEFS_TEST_XV_{SHIP_FAIL_AT,
  CRASH_AFTER_STEP, SERVE_SLOT_MOVED_ONCE}` read once into `ops.Seams`.

## Contracts (in-process, two holders over the loopback hub; `xv/two_holders.zig`)

create in a foreign directory (1 intent, 1 ship, 1 guard RPC, the exact-sum phases); the
byte-exact tree over unlink/rmdir/link/rename/exchange (11 intents minted ≡ retired, shipped
≡ served, no open intent); a failed ship leaves the intent open and the cadence completes it;
the three crash windows roll forward to exactly one outcome (recovery idempotent across
mounts); `SlotMoved` re-dispatched once; the live refusal compensates (EEXIST, the mint
destroyed) and an ABORTING record is inverted at recovery; the `dir_rename` lease serializes
two holders over the wire and the ancestry walk refuses across holders through `LookupExact`;
travelling guards park one scope at the holder and a served step applies COVERED; the record
ship lands chmod/setxattr/removexattr/publish at the holder with the holder's attr cache +
kernel view invalidated (INVAL then PRUNE), redirects once; striping — the supplied flip, the
stripe-keyed create, the lazy re-home (LWW), the stable merged `readdir` across the flip and
its continuation, the `stat` fold, the one-intent `rmdir`, C17 clean; the census trigger (two
creators past `N_floor`; one creator never; K = 1 never). Linux: the filesystem over a
TWO-MEMBER set (`set_bind.zig`) — cross-member creates are intents, unlink/rename/link/mkdir/
rmdir across members land, a remount recovers nothing; the two-member smoke asserts
`xv_cross_owner_intents_minted > 0 ∧ open = 0`.

## Not in this wave (stated)

The kv_fs routing of a FLIPPED directory (lookup/create through the map, the merged readdir
at the FUSE boundary, the explicit flip command) — `stripe.Striping` is complete and tested over
the two-holder harness, the FUSE binding reads the width in force and filters the markers; a
foreign-slot file's LAYOUT publish at the data plane's publish site (the verb and the served
Put exist); a foreign holder's wire session in `set_bind` (resolves `unreachable_` = EAGAIN
until the wire join lands); the `SupplyStripeInos` wire verb (the supplier is a seam).
