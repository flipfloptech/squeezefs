# Porting spec — fsck C1–C17 and the data scrub: the zero-false-positive ladder, the census, sharding, repair (rung 8)

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

Scope: charter §8 "fsck" (C1–C17; the zero-FP ladder; verify-before-repair; dry-run default;
quarantine-first; never fabricate; `fsck_findings` must be 0 on a healthy volume; the CLI surface
`fsck`/`scrub` `--online|--offline`, `--shards k/N`, `merge-reports`, `--repair [--apply]`),
§5.5 "fsck classes on the forest" (the inode plane scopes out slots a LIVE foreign lessee holds and
the inos a foreign ring window names; a `-o ro` census shard judges only what it leases; C9's era
floor consults tree 0; the census is bounded at its start's watermarks), §5.6 (C8 oracle = the
derived layout walk; one-directional offline repair; C11 report-only), §6.1 (C12 report-only),
§1.8 (must-stay-0 tripwires, never a panic in a handler), §1.10 (fsck clean after every kill);
decision 0002 (the reference's fsck is RETIRED as the oracle for v4 trees — rows 50, 104, 105,
176, 177, 202, 352; OURS is the oracle now; §11 below adds the rows this rung owes).

Reference anchors (`FILE:Lnnn`, all under `/Users/joberdorf/Source/squeezefs/`): `FSCK` =
`src/fsck.rs` (10,637 lines, read whole) · `MAIN` = `src/main.rs` · `IPC` = `src/ipc_service.rs` ·
`JOBS` = `src/jobs.rs` · `JW` = `src/job_wire.rs` · `BA` = `src/block_allocator.rs` · `BE` =
`src/meta_backend/kv/backend.rs` · `REC` = `src/meta_backend/kv/backend/recovery.rs` · `AL` =
`src/meta_backend/kv/alloc_lease.rs` · `NV` = `src/cache/nvme.rs` · `RT` = `src/routing.rs` ·
`EK` = `src/env_knobs.rs` · `DVL` = `docs/design-volume-lifecycle.md` · `DSM` =
`docs/design-symmetric-metadata.md` · `CLOSE` = `.benchmarks/2026-07-22-volume-lifecycle-closing.md`
· tests `T/fsck_tests.rs`, `T/fsck_c9_tests.rs`, `T/fsck_c10_tests.rs`, `T/fsck_c12_tests.rs`,
`T/fsck_repair_tests.rs` (+ the sym/kvmap/refs suites named in §10).
Port anchors (`P/…` under `/Users/joberdorf/Source/squeezefs-zig/src/`): `P/data/block_refs.zig`
(`PBR`), `P/data/kvmap.zig` (`PKM`), `P/data/block_alloc.zig` (`PBA`), `P/data/plane.zig` (`PPL`),
`P/data/sym_planes/{data_bitmap,shared_index,custody}.zig`, `P/meta/sym/xv/stripe.zig` (`PST`),
`P/meta/sym/membership/{recovery,volume_bind}.zig`, `P/meta/sym/slots/lease_table.zig` (`PLT`),
`P/meta/inos.zig`, `P/data/{staging,pack,layout}.zig`, `P/meta/dlm.zig`, `P/cli/cli.zig`,
`P/fuse/handler/kv_fs.zig`, `P/core/knobs.zig`.

## 0. Orientation — one engine, two harnesses, and what the port already holds

- **One engine, two harnesses** (`DVL:L475-477`, `FSCK:L309-313`): `run(ctx, opts)` is the
  detection engine (`FSCK:L1741`); **online** = a job on the live mount reading RAM-authoritative
  state with the suspects machinery armed; **offline** = read-only probe opens, nothing in flight by
  definition, so suspects report directly (`FsckMode`, `FSCK:L387-395`). `--repair` is a WRITER:
  offline it takes the D0-guarded open, never the probe (`FSCK:L10479-10519`).
- **Detection never mutates; findings are the only repair input** (`FSCK:L1739-1740`,
  `L8199-8205`). The report carries a typed `FindingId` per finding (`FSCK:L567-775`); a finding
  without one (an older binary's report) is REFUSED by repair, never guessed from display strings
  (`FSCK:L559-565`, `L8235-8246`).
- **The port already holds the per-class oracles** this rung wraps (the orchestration, the
  ladder, the report, the CLI and the repair engine are the new code — §12):
  C8 `PBR:verifyRefs` (`PBR:L695-728`) + `confirm` (the ONE verdict site, `L776-787`) +
  `repairPlan`/`stageRepair` (`L824-857`); C11 `PKM:verifyBlockMap`/`nextOwner`/`deriveBindings`
  (`PKM:L793-863`); C2/C3/C6 `PBA:fsckBeginScan/fsckBumpEpoch/fsckEndScan/allocationEpochOf/
  inflightContains/fsckSetRefcount/fsckReconcileAccounting` (`PBA:L1600-1666`) and the in-flight
  registry `InflightAllocGuard` (`PBA:L1685-1700`); C6's bitmap oracle
  `data_bitmap.verifyBitmap` (`L773-797`); C16 `shared_index.sharedIndexDrift` (`L453-488`);
  C17 `PST:verifyStripes` (`PST:L669-701`); C13 the content-judged orphan census
  (`volume_bind.orphanImagesFn`, `L873-900`; `recovery.zig:L593-616`); C14 `PLT:settleEntry`
  (`.conflict`) + `liveOnTwoPages` (`PLT:L757-787`); C15 the recovery driver (`recovery.zig:L312`);
  the census walk core `PPL:derivedWalk` (`PPL:L2470`) with the C8 exemption callbacks
  `isOpenPack`/`isInflight` (`PPL:L2451-2464`); C9's watermarks `inos.InoWatermark`/`RecoveryFold`
  (`inos.zig:L57-190`); the quarantine marker `layout.M_DAMAGED` + `error.MappingDamaged`
  (`layout.zig:L213-214`, `L234-235`); the staging marker + bind verdict (C5's oracle,
  `staging.zig:L965-1058`); the mount-time corpse sweep (`kv_fs.sweepCorpsesAtMount`,
  `kv_fs.zig:L1335`; 0002 row 177).
- **No job fabric exists in the port yet** (`grep JobType|duty-cycle src/` is empty): rung 8 lands
  it beside fsck (charter §13 item 8). §7 states what fsck needs from it and nothing more.

## 1. The engine — detect → settle → re-check → (plan → apply)

### 1.1 Options (`FsckOptions`, `FSCK:L397-485`)

| field | meaning | port |
|---|---|---|
| `mode` | `Online` / `Offline` (`L387-395`) | `enum { online, offline }` |
| `scrub`, `scrub_only` | add C7 / C7 alone (`squeezefs scrub`) (`L400-403`) | same |
| `throttle_pct` | KD-3 duty cycle, 0/≥100 = unthrottled (`L404-405`) | the job fabric's one knob |
| `shard: Option<(k, n)>` | offline zero-coordination residue shard, `k < n` (`L406-408`) | `?struct { k: u32, n: u32 }` |
| `settle: Duration` | `SQUEEZEFS_FSCK_SETTLE_MS`, default 2,000 ms (`L347-350`, `L488-494`) | `knobs.intKnob` (§9) |
| `cancel: Arc<AtomicBool>` | cooperative cancel from the fabric (`L410-412`) | `*std.atomic.Value(bool)` |
| `staging_full` | fleet shards scan their OWN staging dirs unfiltered (KD-MW-16, `L413-419`) | retired with the fleet fan-out (§11 Q3) |
| `assume_latched` | the fleet fan-out holds the allocator latch across every shard (`L420-426`) | retired (§11 Q3) |
| `inode_plane`, `inode_plane_only`, `owned_volumes`, `multi_owner` | KD-PV-7/8/16 per-volume OWNER scoping (`L427-485`) | RETIRED — the forest's lessee shard (`DSM:L524`, §2.5) replaces owner scoping (§11 Q1) |

### 1.2 Pass 1 — the concurrent read-only scans (`FSCK:L1779-1951`)

The C1 tree walks, the census, the C4/C5 staging scan and the C9 dentry pass are INDEPENDENT
read-only scans; run as a serial sum they measured **0.28×** the census rate at 1 M inodes (the
G-VL-5(c) miss, `CLOSE:L256-266`). Unthrottled runs spawn them concurrently so the pass's wall
clock is the slowest single walk (`FSCK:L1819-1951`); **throttled runs stay serial** because KD-3's
duty cycle is per WORKER (`L1788-1790`, `L1917-1951`). The C1 fan-out is bounded in waves of
`meta_lanes × 4` (a forest unit is one SLOT TREE and an N-writer set holds hundreds — one task per
unit wedged a two-lane pool, `L1820-1858`). Post-fix the RAM-warm scan BEAT the bare census
(1,815,738 vs 1,419,779 inodes/s, `CLOSE:L264-266`); the device-backed gate row read
scan/census = 0.677 over 1,001,014 inodes (`CLOSE:L282`).

Order after pass 1 (`L1954-2022`): C2/C3/C6 against the live allocator → (unsharded only) C8 →
C11 (a)(c)(b) → C12 → C13 → C16 → C17 → C14/C15 → the inode plane C9 → C10 (`L2024-2111`) →
`counters.suspects = suspects.len()` → **settle** (online: `sleep(settle)` then
`fsck_bump_epoch` on every allocator, `L2114-2123`) → `recheck_suspects` → C7 (a FRESH census
post-settle, `L2127-2143`) → sort + dedup findings → `publish_metrics` (`L2145-2190`).

### 1.3 The zero-FP ladder — four shapes (`FSCK:L284-307`, `DVL:L410-421`)

1. **Per-object classes (C1, C4, C5; the inode plane's per-ino arms)**: suspect → settle window →
   a final re-check **under the object's exclusive 4a lease** (`lock_inode_exclusive`; the DENTRY
   lock class is the PARENT's — `owning_ino`, `FSCK:L7045-7056`). Brief, per-object, never a global
   freeze (`DVL:L412`). The raw per-volume read is deliberate: the routed `getattr` takes its own
   shared 4a lease and would self-deadlock against the exclusive one (the C4 lesson,
   `FSCK:L6390-6392`, `L6429-6432`).
2. **Cross-object allocator classes (C2, C3; C6's aggregate; C8)**: no DLM object pins an
   allocator offset, and two legitimate long-lived "inconsistencies" exist by design — a block is
   allocated BEFORE its map is published (an R5-parked or retry-forever write holds that state past
   any settle), and the mover raises destination refcounts before publishing referencers
   (`DVL:L414`). So: the **allocation-epoch side map** (first filter) → two-epoch survival only
   ESCALATES to the **in-flight registry** → then the mover ledger → then the pack-open ledger
   (`FSCK:L5647-5716`). Age alone is never a verdict (`L294-298`). **Normative order at the final
   escalation: registry-absence FIRST, then re-verify the reference state** — the registry contract
   (an owner deregisters only after its publish is durable AND visible to the reads fsck performs)
   makes both interleavings of a racing publish/deregister safe; reference-state-first would
   false-positive on a healthy just-published block (`L298-303`, `DVL:L418`). The test hook
   `PRE_REGISTRY_CHECK_HOOK` marks that boundary (`FSCK:L1297-1318`, `L5667-5669`; the port's
   `Host.recheck_hook` at `.phase_a_registry`). The port (decision 0003 §AH.2) runs Phase A over
   EVERY suspect before the first Phase C — the memoized fresh census is then read after every
   suspect's registry check (interleaved, the census read for one suspect predated the publish
   that ended the next one's mint) — and the C2-leaked / C3 re-checks read the allocator again
   AFTER their fresh derivation (still tracked, at the judged count for C3, no live owner in
   flight, no mint under this scan): registry-absence, then the references, then the tracking.
3. **The inode plane (C9, C10)**: the era floor (C9) / the `(nlink, ctime)` record witness (C10)
   + the open cross-volume intent exemption (the registry's role here, `open_intent_inos`,
   `L7006-7042`) + ONE fresh dentry pass + the exclusive 4a re-check (§3.9, §3.10).
4. **The map plane (C11)**: the crossing registry (`crossing_in_flight`) BEFORE the lease (a live
   train holds the ino's 4a across sweep → chunks → flip, so waiting on it first could park the pass
   for a PB-class migration), then the exclusive 4a, then the registry AGAIN, then fresh reads
   (`L6657-6661`, `L6662-6680`).

Classes whose fresh evidence is ONE census per confirm pass, memoized (the drift-gauge law,
2026-08-23: the per-suspect re-run paid one full layout walk PER SUSPECT — 1,147 walks on the leg
that found it — and each raw pass fed the must-stay-0 gauge with its transients): C8 (`L5822-5828`,
`L5960-5969`), C13 per meta volume (`L5829-5833`), C16 per data volume (`L5835-5839`), C17 once
(`L5841`, `C17Fresh`, `L4897-4947` — Issue 14: N whole-tree dentry walks for N suspects, 64 on
the crash shape that nominates every stripe), C14/C15 per meta volume (`L5845-5847`).

### 1.4 The allocation-epoch side map and the in-flight registry

Reference: `fsck_scan_active: AtomicBool` + `fsck_epoch_map: scc::HashMap<offset, epoch>`
(`BA:L228-233`); `allocate_block` stores the epoch ONLY while the latch is armed — one relaxed load
when no scan runs, zero hot-path cost (`BA:L2503-2505`; `DVL:L416`); `fsck_begin_scan` bumps +
arms (`BA:L1381-1385`), `fsck_bump_epoch` at re-check (`L1388-1390`), `fsck_end_scan` disarms and
drains the map (`L1393-1396`); `allocation_epoch_of(offset) -> Option<epoch>` (`L1401-1403`). The
epoch is deliberately NOT packed into the per-offset incarnation seqlock (the loom-verified
`incarnation_core`; repacking it would couple fsck into the cache-fill validation hot path,
`DVL:L416`). The registry: `inflight_register` returns a guard held until the publish is durable
and visible; dropping it on unwind is exactly right — a dead owner shields nothing (`BA:L1405-1422`);
`inflight_contains` (`L1425`), `inflight_offsets` (C6's aggregate exemption, `L1435`). The
reference registers MINTS only; the port (decision 0003 §AH.1, 0002 row 532) registers every
RELEASE too, from before its releasing commit until after the block's free — between the two the
allocator tracks a block no record names, and a census inside the window confirmed `[C2] leaked`
(C3 for a shared block).

Port (landed): `PBA.fsck_epoch` + `epoch_map: ?[]Atomic(u64)` lazily allocated at the first scan,
one word per cell (`PBA:L537-538`, `L1600-1608`), stamped at the mint when the scan latch is set
(`PBA:L728`); `fsckBumpEpoch` (`L1610`), `fsckEndScan` (`L1614`), `allocationEpochOf` returns 0
for "none" (`L1620-1623`); `InflightAllocGuard.register/deinit` is a per-cell `inflight` count
(`L1685-1700`) — "fsck VISIBILITY ONLY: dropping it frees NOTHING (KD-OV-11)"; a release site
carrying a binding list registers on the same count (`holdInflight / dropInflight`, a drop with
none held refused, `block_inflight_unheld_drops`; `write_seams.ReleaseHolds`, `MergeOutcome.holds`,
the handler's `AfterCommit` holds, `DataPlane.freeBindings`'s `release_free_unheld` tripwire). `fsckEndScan` does
NOT drain the map (the reference's `clear_sync`): the next `fsckBeginScan` bumps the epoch, so a
stale stamp reads as an OLDER epoch and is judged exactly as "allocated before this scan" — the
port must compare `allocationEpochOf(idx) >= scan_epoch` (younger ⇒ exempt), never `!= 0`
(§12 item 2).

## 2. The census machinery

### 2.1 The census walk (`walk_census`, `FSCK:L3470-3661`)

One paged range scan of the inode kind per volume (`SCAN_PAGE = 512` records per fetch — also the
throttle's duty-cycle unit, `L354-356`) + one `layout` read per live ino + the kvmap extraction for
tree-mapped heads (`kvmap_entries_for`, `L3667-3687`; the SHARED extraction so both C8 oracle sides
read one multiset — Rev 1.1 #3, `L3626-3631`). Per layout `census_layout` counts ONE reference per
map entry — inline, indirect (the blob block itself included, `block_idx = u32::MAX`), or the
tree's records — mirroring `recover_active_blocks_v3`'s accounting exactly (`L3689-3772`); every
mapping lands in `refs[vol][offset] += 1` and `mappings: Vec<MappingRef>`; one that resolves to no
backend lands in `unresolvable` (C2-lost by construction, `L5494-5505`).

**Bounded at its start's watermarks** (PR 13i §4.4aw, `L3488-3506`): `slot_ino_watermarks()` is
read at the walk's start; a record minted past its slot's ceiling is skipped, and a page whose last
record lies at/past it JUMPS the cursor to the next slot (`L3547-3556`) — a walk paged at 512 with a
`layout` read per ino never caught a creator that outpaced it. A post-start record is the next
census's: invisible to `live` and the block census, judged by the ladder's era floor, fresh re-read
and epoch exemptions. **A slot with no watermark (a keyspace this mount holds no LIVE cursor for) is
walked whole** (`L3505-3506`; `BE:L12332-12410` — the first build bounded joiners' slots at the
grant-time words and read their live blocks as C2 "leaked" after a manager failover, §4.4ba; a
reader's census skipped a live writer's rotor past its last page word, §4.4bk).

**The corpse-census correction** (2026-08-23, `L3566-3577`): an `nlink == 0` record (unlinked, its
FORGET not yet arrived) is LEGAL live state that still OWNS its blocks — it stays OUT of every
inode-plane set (C9's live bitmap, C10's count map) but its layout DOES feed the block census. The
old whole-record skip read every such block as C2 "leaked" and its ledger record as C8 drift: **152
false positives on a healthy fleet seconds after a tar-x/rm pass**. The port's
`block_refs.OwnerBindings` doc already says "corpses INCLUDED" (`PBR:L653-654`).

Also carried per visited inode: the C9 `live` bit (`L3590-3595`), the C10 `odd_nlink` map
(non-directory, `nlink != 1`; budgeted — over budget ⇒ `odd_nlink_complete = false`, the count arms
record no verdict, `L3596-3608`), the C11 (b) nominations (a sweep-less `kvmap:` head with nonzero
size and ZERO records; corpses never nominate — a truncate/unlink sweep legitimately leaves the
shape in range, A2; `L3632-3648`), and `complete = false` on cancel/unreadable node (`L3509-3530`).
A guest-only member's raw CONTROL records have no global encoding and are skipped (VL9 soak-found
panic, `L3578-3582`). Under `--shards`: `global_ino % n != k ⇒ skip` (`L3583-3587`).

Port: `PPL.derivedWalk` (`PPL:L2470`) is this walk's layout half; the port's inode-major slot tree
walks every kind in one pass (`DSM:L710` "a kind pass IS the census walk": ≈ 3× the bytes of a
dentry-tree pass but ONE walk for C1–C10 and C13/C16/C17). The port's census therefore visits each
leaf once and dispatches per kind (inode → `live` bit + `odd_nlink`; dentry → the referenced bitmap
+ markers; layout xattr → bindings; refs → the durable half of C8; kind 7 → C11). Resume cursors
follow 0002 row 176 (`key.sameKindSuccessor` / the next ino), never `key ‖ 0x00`.

### 2.2 `InoBitmap` — C9's ino set (`FSCK:L977-1127`)

One bit per inode ever allocated, dense per keyspace: a global ino decomposes to `(slot, raw local)`
through the durable routing width (`route_ino_width`, the SAME decomposition on both sides — a
dentry's target and an inode record's own ino — so a slot migration mid-scan cannot shift a bit,
`L980-986`); raw locals are monotonic from 2 and never reused, so each keyspace's bits are dense:
≈ 12.5 MiB per set at the ≥ 100 M-inode cap vs 4.8–6.4 GB for a `HashSet<u64>` (`L986-991`). Only
populated slots allocate a word vector (`slots: HashMap<slot, Vec<u64>>`, `L1002`). `raw < 2`
never indexes (raw 1 = the root pin / a control record — the root exemption falls out of the
encoding, `L993-996`, `L1031-1041`). `raw_ceiling` = `max_local_ino_watermark()` folded over the
set (`BE:L12289-12311`): **a dentry's `child_ino` is never an allocation authority** — a corrupt
value naming `u64::MAX` would otherwise size a bit vector from it (the `kv_bset` `record_count`
lesson, `L1010-1016`). `byte_budget` (§2.6) exceeded ⇒ `truncated = true` and C9 records NO
verdict (`L1050-1057`, `L1069-1074`). `mark` returns whether the bit was NEWLY set (`L1043-1067`)
— what makes C10's counting free for the single-name majority. `each_absent_from` is word-parallel
`AND NOT`: a healthy volume's whole difference is `population / 64` words with no per-inode work
(`L1097-1126`).

Port: `fsck/ino_bitmap.zig` — `std.AutoHashMapUnmanaged(u16 slot, []u64)` keyed by the forest slot
(`inos.routeIno`/`makeGlobalIno`, `inos.zig:L303-318`), the same API; the budget charged per
`resize`. Tie test: 1 bit per ino, root never indexed, a ceiling-crossing ino marks nothing
(`T/fsck_c9_tests.rs:L403` `test_ino_bitmap_is_one_bit_per_inode_and_indexes_no_root`).

### 2.3 The dentry pass (`build_referenced_inos`, `FSCK:L3877-4062`)

ONE sequential pass over every volume's dentry kind marking `DentryValue::child_ino` (the GLOBAL
ino — a dentry lives on its PARENT's volume and may name a child on another, so the union across
volumes is the referenced set, `L3851-3856`). Spawnable: disjoint from the census and the C1
walks. Returns `None` on an unreadable node / cancel / a truncated set — a truncated referenced set
would make every inode named beyond the tear look unreferenced, so C9 records NO verdict for that
run (`L3862-3866`, `L4050-4058`). Rides the same walk: **C10's `multi` map** (dentry RECORDS naming
each ino with more than one — counted only for inos whose FIRST name marked a bit, so a hostile
child ino is never counted, `L4009-4024`; over budget ⇒ `multi_complete = false`), **C10's
`names` identities** for the caller's `collect` set, deduped by `(global parent, name)`
(`L4027-4047` — a VL5b slot migration mid-copy has one dentry on two volumes, so RECORDS may only
NOMINATE and every verdict is from the deduped identities, `L3874-3876`), and **C17's marker
census** (every NUL-led name, bounded by `K + 2` per striped directory, `L3997-4003`).

Sharded (`child_ino % n == k`, `L4004-4008`): the module's **shard rule** — a shard covering a subset
of INODES still needs the full referenced set for those inodes, so it filters by the ino the
VALUE carries, never by the key's parent; every shard walks the whole dentry tree but marks only its
own residue (`FSCK:L315-324`).

### 2.4 The era floor — and tree 0 as its witness (`minted_in_prior_era`, `BE:L12440-12454`)

`true` ⇔ the record was minted BEFORE this mount's writer era began: inos are monotonic per
keyspace and never reused, the era's floor is the watermark recovered at open, so the statement is
exact provenance, not a heuristic (`BE:L12412-12424`). A create legitimately commits the inode
record before its dentry, and every ino this mount can mint is at or above the floor — so the floor
alone makes "an inode with no dentry" safe against live work; the honest consequence: residue
created during THIS mount is reported by the NEXT mount's scan (`FSCK:L147-158`). **On a forest tree
0 is a witness beside the floor** (Issue 2): a slot NOBODY leases has no minter, so every record in
it is prior-era whatever the floor says (a joined appender's rotor slots never reach the manager's
cursors, so its mints read current-era at every censusing mount — F-R3's orphans passed the class);
a PROBE has nothing in flight, so every guest record it reads with a word is prior-era too; a slot
a LIVE foreign appender leases stays fail-closed (`BE:L12425-12434`). Fail-closed for a guest
keyspace this mount had no cursor for at open (a virgin slot, a migrated slot's cursor arriving
mid-mount): `false` — no verdict rather than a guess (`L12436-12439`).

Port: `inos.InoWatermark.snapshot()` at open per keyspace = the floor (`inos.zig:L86`;
`recoverNextIno`, `L99`); the slot plane's `lease_table` view + tree 0's `slot_state` words answer
"unleased" (`PLT.view(slot).state == .unleased`); the port adds `fsck/era.zig: priorEra(ino) ->
enum { prior, current, unknown }` (3-valued, fail-closed).

### 2.5 Forest scoping — foreign slots, foreign windows, the `-o ro` shard

- **Slot coverage** (`SlotCoverage`, `AL:L1311-1344`): `leased` (this mount's, native included),
  `unleased` (the manager's to walk), `foreign`, `foreign_live` (a lessee the installed membership
  owner lists LIVE — its tree CHANGES under a census here), and **`unjudged_slots`** — the ONE
  predicate the dentry pass and the raw C1 walk read: on the MANAGER the `foreign_live` slots (a
  lessee NOT known live is PR 10's frozen-tree class and IS judged); on a MEMBER (a joined appender,
  a `-o ro` reader) every slot NOT leased by this mount — the manager's rotor roots ride its page
  and its SMOs re-root the unleased trees between projection refreshes. Coverage is INCOMPLETE where
  non-empty: counted (`fsck_c1_projection_slots_scoped`, `fsck_inode_plane_foreign_dentry_scoped`),
  never a finding (`FSCK:L3262-3320`, `L3896-3932`). WHY: a raw walk from the manager's root word
  over a lessee's tree read a routing loop (`root-seq` restarts to the budget) and reported a false
  `C1Torn` over a healthy tree; the `sym-storm` leg read a remounted joiner's **1,444 removals as 442
  dangling names** (`L3903-3906`). A coverage read that FAILS walks nothing (an unwalked tree is an
  incomplete pass; a projected tree's loop is a false finding — the honest one, `L3278-3292`).
- **Per-ino slot scoping** (`inode_plane_owns_slot`, `AL:L2464`): a C9/C10 candidate in a slot
  another appender leases is its LESSEE's candidate (`fsck_inode_plane_foreign_slot_scoped`,
  `FSCK:L4119-4124`) — `DSM:L524`: "KD-PV-16's owner shards ⇒ lessee shards; coverage
  `fsck_inode_plane_slots_covered ≡ leased ∪ tree-0 unleased`".
- **Foreign ring windows** (`foreign_window_inos`, `REC:L876-952`): every `Live`/`Recovering` page
  of an appender this mount is NOT (and did not replay) with a non-empty ring: read its window once;
  every slot-tree inode key and every dentry's child (a DELETE's child resolved through the tree,
  which still holds the pre-delete dentry — Issue 26: a cross-owner unlink's name removal rides the
  parent holder's ring while the child's `nlink` decrement landed in the child's slot, read as C10
  LOSS until the holder's checkpoint, `REC:L910-927`) is scoped out of C9/C10
  (`fsck_inode_plane_window_scoped`, `FSCK:L4125-4131`). A ring that cannot be read ⇒ unknown =
  pending, the WHOLE plane takes no verdict (`L4953-4969`).
- **The `-o ro` / probe rule** (`FSCK:L3942-3965`): on a non-writer, any `Live` appender page at
  open is an UNCOVERED ring window (acked records replayed by a WRITER's own-residue open and by
  nothing else) ⇒ the inode plane records no verdict (Issue 21b). Charter §5.5: "a `-o ro` census
  shard judges only what it leases" — a reader leases nothing, so its block-plane shard covers its
  own slot trees' residue and the inode plane is the writers' (§11 Q3 keeps this law, retires the
  fleet wire it rode).

### 2.6 Derived budgets (`FSCK:L1430-1465`)

- `ino_set_byte_budget() = max(mem_budget / 64, 16 MiB)`: a scan holds two sets (≈ 3 % of the
  budget transiently); the floor is the design cap's own requirement — 100 M inodes / 8 = 12.5 MB of
  bits, 16 MiB with headroom for the sparse tail (`L1430-1443`; tie test
  `T/fsck_c9_tests.rs:L1039`).
- `c10_count_entry_budget() = ino_set_byte_budget() / C10_COUNT_ENTRY_BYTES`, `C10_COUNT_ENTRY_BYTES
  = 64` — the accounting factor converting the byte budget into entries (a `(parent, name)` pair or
  an `(ino, nlink)` pair with hash overhead; short names round down, a 255-byte name rounds up — a
  bound, not a promise; `L1445-1465`; tie test `T/fsck_c10_tests.rs:L1469`). Exceeding it makes the
  count arms record NO verdict while the DANGEROUS arms keep working (they read only bitmaps).
- The candidate ceiling per class per run = `ino_set_byte_budget() / 8` entries; the rest is
  DEFERRED and said loudly — batching is convergent (repair the reported ones, re-run; `L4076-4101`,
  `L4309-4326`).

Port: `derive.Facts`-based functions in `fsck/budget.zig` with drift-is-red tie tests
(AGENTS.md "Derivations").

## 3. The classes

Each row: what it reads · the invariant · the finding record (`FindingId` variant and its fields,
`FSCK:L567-775`) · the ladder · online vs offline · repair (plan verb `FSCK:L7740-8019`; apply
`L8406-10420`) · the port's oracle.

### 3.1 C1 — tree integrity (`FSCK:L3223-3435`; recheck `L6034-6096`, `L6333-6370`)

Reads: every C1 unit paged from `[0x00]` to `KEY_SPACE_MAX` at `SCAN_PAGE` (`c1_page`,
`L3325-3335`): a per-kind tree on a flat volume; ONE SLOT TREE read RAW with every kind in its
mixed leaves on a forest (`C1Unit`, `L3251-3260`) — the raw read is the only one that can SEE a key
the forest codec refuses (kind-routed walks skip such a record so a census never truncates).
Checksums verify on the read (ride-along). Invariants: (a) the walk advances (an unreadable node ⇒
`C1Walk`, the walk STOPS there, `L3364-3377`); (b) stored keys strictly ascending in- and
cross-page (`C1Record "key ordering violated"`, `L3405-3418`); (c) per record the schema
(`record_schema_violation`: inode/dentry/xattr key decode + value decode, `L3223-3249`); (d) on a
forest a slot-tree key `split_forest_key` refuses ⇒ `C1RawKey` (`L3389-3403`).
Findings: `C1Torn{vol, tree, slot?, cursor_hex}`, `C1Semantic{vol, tree, key_hex}`,
`C1RawKey{vol, slot, key_hex}` (`L573-594`).
Ladder: `C1Walk` → re-read the SAME page shape from the cursor (a `max = 1` probe could be
satisfied by a healthy left sibling and never touch the damaged node, `L3322-3324`; transient I/O
clears, `L6041-6065`); `C1Record` → final `lookup_kind` under the owning ino's exclusive 4a where
the key names one (online), vanished = cleared (`L6340-6369`); `C1RawKey` → `slot_tree_lookup_raw`
(`L6072-6095`). Projection slots are scoped out (§2.5).
Repair: `C1Torn` → `quarantine-report-only` (no replicas exist; identity + error into the
manifest, no mutation; the finding PERSISTS by design — data loss made visible, never fabricated
away; `L7742-7747`, `L9591-9651`); `C1Semantic` → `rebuild-in-place` (quarantine key+value, then
`delete_kind` — an ordinary journaled CoW leaf re-emit; refused if the record now satisfies its
schema, `L9712-9779`); `C1RawKey` → the **deletion gate** `raw_key_repair_gate` (`L7712-7738`):
a KNOWN slot-tree kind in a shape it never takes (`MalformedKnownKind`, `SlotOutOfNamespace`) is
dropped in place; an `UnknownKind` (a LATER binary's record under a new incompat bit?) or a
`Truncated` key is REPORT-ONLY — "the raw repair never deletes what it cannot name"
(`L9653-9710`). `DVL:L445`'s optional `--drop-damaged` is NOT built (§11 Q8).
Port oracle: `meta/tree.zig` range walk + `meta/node.zig`/`bset.zig` verified loads (checksum
failure is `error.Corrupt` at the page read); `meta/key.zig` kind decoders + `record.zig` value
codecs; the raw slot-tree page read exists in the tree API.

### 3.2 C2 — map ↔ allocator: leaked / lost (`FSCK:L5281-5381`, `L5494-5505`; recheck `L5852-5926`)

Reads: the census refs per canonical volume id vs `tracked_offsets()` (the RAM refcount map),
`capacity_bytes`, `chunk_size` (`L5290-5294`). **Leaked** = tracked with zero referencers
(`L5321-5327`); **lost** = referenced but past capacity / not chunk-aligned / not tracked /
unresolvable (`L5345-5381`, `L5494-5505`). Leaked and C3 need allocator ground truth that is
mount-session RAM — meaningless on a shard (a shard sees one residue's references) and on offline
probes without the recovery walk (`L5315-5318`; offline full runs rebuild it via
`recover_active_blocks_v3`, `L10585-10598`); so a shard judges only the lost/out-of-range arm
(`FSCK:L324-328`). A referencer that is a `damaged:` quarantine marker is never lost — the mapping
IS the repair (`L5342-5355`). **On a grant-armed allocator whose lease THIS process holds**
(defect 27): a referenced offset untracked in RAM but SET in the holder's bitmap is a former
lessee's mint — the bitmap is the allocation truth; `fsck_alloc_bitmap_tracked_exempted`, not lost
(`L5295-5313`, `L5373-5376`).
Findings: `C2Leaked{vol, offset}`; `C2Lost{vol, offset, ino, block_idx, mapping,
unrepairable_shape}` (`L596-608`; `unrepairable_shape` = out-of-range/unaligned/unknown backend —
the content-verify arm is skipped, the action is quarantine).
Ladder: Phase A epoch filter (`allocation_epoch_of(offset).is_some()` online ⇒
`epoch_exempted`), hook, registry (`inflight_contains` ⇒ `inflight_exempted`), mover pre-publish
ledger (`mover_ledger_exempted`), pack-open ledger (`pack_ledger_exempted`: the +1 PIN of an OPEN
pack reads C2Leaked before its first tenant commits — resolved to `(allocator, offset)` through the
router, never by key spelling, `L5624-5644`, `L5654-5686`). Phase B ONE fresh census
(`L5718-5735`). Phase C: leaked ⇒ still tracked ∧ still unreferenced (`L5852-5869`); lost ⇒ the
§5.6 normative order for the mover's src-free adversary — observe the ALLOCATOR first, then
re-verify the REFERENCE with a fresh per-ino layout read (`current_mapping_present`): a drain frees
a source only after every referencing publish is durable, so the moved-and-freed race always shows
the NEW mapping and clears (the leg-13 drain-concurrent FP shape, `L5895-5925`); unresolvable is
permanent by construction (`L5879-5893`).
Repair: leaked → `free-leaked-block` (quarantine the block bytes best-effort, then `free_block`:
begin → purge → punch → finish; refused if tracked no more, referenced or in-flight now, OR **if the
durable ledger still names the block** — that block is C8's to judge: freeing it under a standing
record would seed it LIVE at the next mount while the bitmap reads clear — the allocation arm's loss
refusal manufactured by the remedy verb, `L9781-9897`); lost → `unrepairable_shape` ⇒
`quarantine-mapping` (flip to the `damaged:` marker — reads EIO, a hole is never silently
fabricated); else `verify-content-then-repair-allocator-or-quarantine`: content verifies (the C7
check) ⇒ `recover_block` + `fsck_set_refcount(counted)` + `publish_block` (the data was fine, the
accounting was wrong); else quarantine the mapping (`L7765-7790`, `L9899-10013`). The flip rides
`BlockMapOp::MergeExpected` — supersession-safe: it lands only where the captured mapping is STILL
current (`flip_mapping_damaged`, `L8141-8172`).
Port oracle: `PBA.fsckReconcileAccounting` (`PBA:L1639-1666`) is the MOUNT-time C2 (`PPL:L2443-
2447` logs "leaked freed / lost evicted"); the online class reads `PBA.Refcount.peek` per cell vs
the census; the lost arm's "SET in the holder's bitmap" reads `data_bitmap.isSet`; the damaged
flip = a layout publish of `Mapping{flags |= M_DAMAGED}` through the layout plane's merge-expected
op (`layout.zig:L213-214`).

### 3.3 C3 — refcounts (`FSCK:L5328-5336`; recheck `L5927-5947`; repair `L10015-10111`)

Reads: `tracked[offset] = rc` vs `refs[offset] = n` (unsharded, online + offline-full).
Invariant: `rc == n` for every tracked referenced block (clone-aware: N tenants/clones = N
references). Finding: `C3Refcount{vol, offset}`. Ladder: Phase A as C2 (epoch → hook → registry →
mover ledger → pack-open ledger: an open pack reads `rc = tenants + 1`), Phase B fresh census, Phase
C `expected > 0 ∧ actual != expected` (untracked/unreferenced shapes are C2's, `L5944`).
Repair: `recount-and-set-refcount` — every referencing ino's exclusive 4a lease taken ASCENDING,
the references recounted UNDER them through the census extraction, `fsck_set_refcount(counted)`;
healed/untracked/unreferenced refuse (`L10015-10111`). Port oracle: `PBA.fsckSetRefcount`
(asserts `count != 0`, `PBA:L1629-1632`); `dlm.Manager.lockMany` for the ascending canonical
acquisition (`dlm.zig:L373`).

### 3.4 C4 — orphan staged custody (`FSCK:L5570-5603`; recheck `L6371-6411`; repair `L10113-10211`)

Reads: `scan_live_staged_custody(dir, CUSTODY_SCAN_MAX = 1,000,000)` — a header scan of the
`staging_segment/` shard files; sound because removal/flush zeroes each record's magic, so only
never-flushed custody carries a live header (`NV:L184-197`); keys
`active_block[_ext]:inode_{ino}:block_{b}` parsed by `custody_key_ino` (`FSCK:L5513-5520`).
Invariant: the ino has live meta (`getattr` ok with `nlink > 0`); `NotFound` or `nlink == 0` ⇒
suspect; an infrastructure error is never a finding (`L5588-5592`). Finding: `C4Orphan{dir, key,
ino}`. Ladder: per-object — exclusive 4a on the ino (online), custody still present ∧ ino still
missing through the RAW per-volume `getattr` (`L6371-6411`). Sharded by `ino % n == k` unless
`staging_full` (`L5583-5587`).
Repair: `quarantine-then-discard-custody` — `extract_and_kill_staged_custody(kill = false)` copies
the verbatim record image(s) (header + key + payload) into quarantine, hook, then the live store's
`remove_active_block_async` (a blocking-pool hop: the shard WRITE lock legitimately waits for read
guards with await-side lifetimes — the VL8 generic/464 wedge family) and the raw on-disk retire
(`kill = true`) (`L10113-10211`).
Port oracle: `staging.Segment.recover/residents/liveExtents` (`staging.zig:L579-655`) +
`StagingKey.ino()` (`L153`); the record header carries `generation` + `fencing` (`L127-139`); a
"kill" = `Segment.remove(key, expect_generation)` (`L463`). v4 custody is binary
(`StagingKey{file_id, ino}`), not a string key — the finding's `key` is `(file_id, ino)` (§11 Q6).

### 3.5 C5 — staging validity (`FSCK:L5533-5568`; recheck `L6962-6978`; repair `L10213-10296`)

Reads: `read_staging_generation_marker(dir)` vs `expected_generation` = `volume_generation(meta)`
(`v3:` ‖ hex uuid per volume joined by `|`, decorated with the writer scope when every member
carries bit 10 — what the markers actually hold, `FSCK:L1266-1291`). Invariant:
`marker_is_rebindable(found, expected)` (`Match | ScopeUpgrade` — an un-scoped root of a now-scoped
set is the pre-upgrade shape the next mount adopts, not a finding, `L5536-5540`); `None` + segment
data present ⇒ suspect; unreadable ⇒ suspect. Finding: `C5Staging{dir}`. Ladder: re-read the marker
post-settle (a live restamp clears, `L6962-6978`); per-shard-repeated, deduped at merge.
Repair: `quarantine-staging-dir` — copy marker + every segment file (fsync each + the manifest)
BEFORE any removal, then remove the originals (the discard-on-mismatch law made non-destructive,
`L10213-10296`).
Port oracle: `staging.Marker.decode` + `bindVerdict(found, ours, peer_live)` (`staging.zig:L1008-
1058`): `.match/.upgrade` = rebindable; `.dead/.foreign_scope` = the finding. The v4 marker is a
72 B checksummed `extern struct` whose identity is `xxh3(set_uuid ‖ "staging")` (`L948-958`) — the
reference's text generation string is a quirk (§11 Q6).

### 3.6 C6 — capacity accounting (`FSCK:L5383-5489`; recheck `L6015-6033`; repair `L10298-10327`)

Two arms, unsharded only. **Flat / un-armed**: `used = highest_block_index − free_blocks_count −
inflight(untracked) − graced` vs `tracked.len()` (`L5456-5489`; the port's in-flight term skips a
free-listed and a trim-windowed block — `freeBlocksCount`'s and `trimClaimed`'s already, so a release
held past its free is not counted twice, decision 0003 §AH.3): in-flight-registered offsets are
"used" by the arithmetic but deliberately untracked — since the write-wall manners law the
`begin_free → reclaim` limbo legitimately spans whole foreground-busy periods, so the settle window
can no longer absorb it (the 2026-07-31 iteration-loop FP); freed-offset GRACE entries are likewise
untracked and not free-listed (spec §6.8 item 3 — without the exemption every reader-armed mount
reported C6 drift while churning). **Grant-armed (`block_grant_armed`)**: the bitmap IS the free
list, so `highest − free_list` is single-writer arithmetic that reads every bitmap-clear block below
the cursor as used (the `sym-crash` leg read one crash's released window as a **386-vs-385**
finding); C6 is then PR 8's **bitmap oracle** `DataAllocBitmap::drift(referenced, open_ranges)`:
LOSS (referenced ∧ clear — a future carve would overwrite a live block) is the report-only finding;
LEAK (set ∧ ¬referenced ∧ ¬granted ∧ ¬in-flight) is a dead incarnation's remainder the next
(re-)hold releases, counted on `fsck_alloc_bitmap_leak_candidates` (`L5392-5441`). A grant-armed
allocator whose holding this process does not keep (a wire writer) is DECLINED, counted
`fsck_alloc_census_declined` (`L5443-5455`).
Finding: `C6Drift{vol}` (evidence carries `used` vs `tracked`). Ladder: re-derive post-settle
(`L6015-6033`). Repair: `recompute-accounting` → `fsck_reconcile_accounting()` → `(frees_completed,
evictions)` (`L10298-10327`).
Port oracle: `data_bitmap.verifyBitmap(referenced, granted, leaks)` (`L773-797`; LOSS fires the
`data_alloc_bitmap_drift` tripwire, LEAK counts `data_alloc_bitmap_leak_candidates`,
`L791-795`); `PBA.fsckReconcileAccounting` (`PBA:L1639-1666`: untracked ∧ allocated ∧ no in-flight
∧ not graced ⇒ complete the free; tracked ∧ free ⇒ evict). The port's allocator is ALWAYS
grant-armed under the symmetric default (0002 row 292), so the flat arithmetic arm exists only for
`--single-writer` volumes.

### 3.7 C7 — the data scrub (`FSCK:L7071-7317`; repair `L10329-10419`) — see §4.

### 3.8 C8 — durable-vs-derived reference drift (`FSCK:L4467-4495`; recheck `L5948-6014`; repair `L8492-8753`)

Reads: `verify_durable_block_refs` = the ledger population per `(vol_tag, block_idx)` vs the
census's derived count — through the same key resolution, so a disagreement is divergence, not a
race (`L1957-1962`). Unsharded only (a shard's census is not comparable, `L1964-1965`); skipped when
no volume has the ledger engaged (the port's ledger is ALWAYS ON — 0002 row 104, `PBR:L37`).
**Detection is UNGATED here while MOUNT keeps it behind `SQUEEZEFS_BLOCK_REFS_VERIFY=1`**: fsck
walks every inode anyway, so the class adds one paged range scan of the reference kind per data
volume (≈ 2.9 ns/reference to decode) + a fold and diff (≈ 90 ns/reference —
`.benchmarks/2026-08-04-durable-block-refcounts.md` §3–4), no walk fsck did not already owe
(`L1975-1985`). Finding: `C8DurableRefDrift{vol, offset}` (`L616-622`). Ladder: ONE memoized fresh
comparison for the confirm pass; still drifting ⇒ pack-open ⇒ `pack_ledger_exempted` (the small-file
packer made ONE block the landing site of every promotion in a batch, so the open pack re-drifts by a
DIFFERENT commit on both passes — the two-epoch check cannot exclude it, `L5970-5990`); in-flight
owner ⇒ `inflight_exempted`; else the finding fires `META_KV_BLOCK_REFS_DRIFT` — **a CONFIRMED
finding is what feeds the must-stay-0 tripwire; the raw comparison observes and convicts nothing**
(`L5954-6013`).
Repair (`release-stale-block-refs`, `L8492-8753`): OFFLINE ONLY (online REFUSES — the verification
read and the release must not interleave with a live publish; the ledger commit takes the owner's 4a
itself so the two cannot be one lease-hold online, `L8484-8505`). Every record of the block is
judged BEFORE any is released; a justification that cannot be READ refuses the WHOLE finding (a torn
indirect blob makes the walker under-count and the oracle reports `durable N vs derived 0` for every
block of the file — releasing those would drop the file's only pointers, `L8583-8590`); an owner the
routed set cannot NAME refuses (routing a raw control local as a global would read an unrelated
inode's layout — the first build's data-loss shape, `L8521-8544`); `LayoutNames::{Named, NotNamed,
Unknown}` three-valued (`L8658-8694`); the OTHER direction (a layout naming a block with no record)
stays report-only — a record is never fabricated (`L8696-8707`). One `commit_block_refs_witnessed`
per stale record under its owner's 4a; a failure refuses THIS finding naming what landed
(`L8708-8742`).
Port oracle (landed): `PBR.verifyRefs` with `Exemptions{isOpenPack, isInflight}` — each face judges the
verified volume's block, never the index on any volume (packfsck) (`PBR:L683-728`),
`PBR.confirm` = the verdict site firing `meta_kv_block_refs_drift` and logging the first
`DRIFT_LOG_ROWS` offenders WITH owners (`L776-787`), `PBR.repairPlan(…, online)` refusing
`OnlineRepairRefused`/`UnknownJustification` with `Naming` three-valued and `unfabricated` counted
(`L790-849`), `PBR.stageRepair` (`fsck_repair_class_c8` per record, `L852-857`); `PPL.verifyRefsAtMount`
is the mount-time arm (`PPL:L2421-2449`). Test: `PBR:L1220`.

### 3.9 C9 — unreferenced inodes (`FSCK:L112-174`, `L4068-4182`; recheck `L6412-6462`; repair `L9059-9236`)

Reads: `census.live` (inodes with `nlink > 0`) minus `refs.refs` (the dentry pass's targets) —
word-parallel `each_absent_from`, candidates capped at `budget / 8` with the rest deferred loudly
(`L4076-4101`). Three shapes reach it (`L118-129`): the crashed cross-volume create (`nlink == 1`,
no dentry, no blocks); pre-S3.5 cross-volume `link`/`unlink` field damage (the same shape PLUS every
block the inode owned — the only way an operator learns a volume carries it); S9 causes (a dead
writer's in-flight create; a recovered plan whose `MintInode` applied while its `InsertDentry`
volume was fenced). Nothing else sees them: reclaim admits only `nlink == 0` FORGET'd inos and the
kernel never learned this ino exists; C2/C3 ask about BLOCKS and an unreferenced inode's layout still
names its blocks (`L130-136`). **Deliberately out of scope: `nlink == 0` unreferenced** (POSIX
unlinked-but-open / POSIX-15's rename-overwrite orphan — needs the live open-count/reclaim
registries; claiming it would trade a leak for destroying an open file, `L165-173`; in the port the
mount-time corpse sweep owns that shape — 0002 row 177).
Per-candidate gates in order (`L4109-4155`): owner scoping (RETIRED, §11 Q1) → lessee scoping
(`inode_plane_foreign_slot_scoped`) → foreign window (`inode_plane_window_scoped`) → **THE guard**
`minted_in_prior_era` (`current_era_exempted` — nonzero under concurrent creates is the proof the
shield works) → the open-intent set, read lazily (`unreferenced_intent_exempted`: a cross-owner
create commits the child's record before its `InsertDentry` ships; once the child's slot reads
UNLEASED tree 0 makes it a prior-era candidate with no name — the roll-forward's object, never C9's,
Issue 10) → a fresh record read (vanished ⇒ no verdict; `nlink == 0` ⇒ out of scope) → the
evidence `blocks = layout_mappings_of(ino).len()` (`blocks > 0` is the pre-S3.5 signature).
Finding: `C9Unreferenced{ino}` (the global ino IS the identity; everything else is re-derived under
the lease at verify time, `L629-634`). Ladder (`L6412-6462`): intent exemption → exclusive 4a on the
ino (online) → still live (`nlink > 0`) ∧ still prior-era ∧ still unnamed in the FRESH dentry pass
(`Some(Some(pass))` → contains?; `Some(None)` = an INCOMPLETE fresh pass CLEARS; `None` = offline,
nothing in flight — the three-way gate, `L5800-5806`, `L6446-6453`). The fresh pass is the arm that
catches the one live way a name can appear for an unreachable inode — an `open_by_handle_at`
reconnect or a plan's late `InsertDentry` (`L5740-5745`).
Repair (`destroy-unreferenced-inode`, the most destructive act in the surface, `L7826-7837`,
`L9059-9236`): intent ⇒ refuse → verify under the lease (live, prior-era, the repair-time dentry
pass does NOT name it; an incomplete pass REFUSES — "rather than destroy an inode whose name may
exist") → quarantine the inode record + EVERY xattr record (key AND value — the layout is the only
map to the blocks) with the block keys enumerated in the manifest (block CONTENTS are not copied:
unbounded — the manifest states what was reclaimed rather than pretending to keep it) → hook →
`delete_file` (durable references released, blocks through the terminal-free law; `NotFound` =
the crashed-create shape; any other failure LEAVES the record so the next run retries) →
`destroy_unreferenced_inodes` in ONE journaled transaction. Step order is chosen for its crash
windows: interrupted between data and destroy the inode is still `nlink ≥ 1` and unnamed, re-detected
as C9, both steps idempotent (`L9040-9058`). **The inode plane runs FIRST at repair, C9 before C10,
then everything else** (`L8346-8370`): a C9 destroy deletes a REFERENCER, so every block-class
finding naming its blocks verifies as healed instead of paying quarantine work on a doomed inode.
Port: the census bitmaps (§2.2), the era function (§2.4), `xv/intent.zig` open intents (the
`ABORTING` bit — 0002 row 272), `inos.stageDestroy` (`inos.zig:L236`) for the destroy rider.

### 3.10 C10 — nlink / name consistency (`FSCK:L175-283`, `L4194-4425`; recheck `L6467-6655`; repair `L9259-9589`)

Two halves. **The count arms** (gated on `multi_complete ∧ odd_nlink_complete`, `L4206-4291`):
side A = census `odd_nlink` (non-directory live inodes, `nlink != 1`) vs `record_names_of(ino)`
(the `multi` map, else 1 iff the bit is set); side B = `multi` inos side A did not judge, one record
read settles directory/`nlink == 0`/mismatch. No name at all is C9's object, never C10's
(`L4234-4238`). **The dangerous arms** = the REVERSE difference `referenced \ live` (the census skips
`nlink == 0`, so this is exactly {an inode whose count is 0 while a path still resolves} ∪ {a name
whose inode does not exist} ∪ {the census/pass concurrency window}), split by one fresh record read:
`Ok(None)` ⇒ dangling; `nlink == 0` ⇒ `C10ZeroNlinkNamed`; live ⇒ `nlink_transient_cleared` (the
create window) (`L4293-4371`). An incomplete census ⇒ NO verdict for the reverse arms (a missing
live inode would make its names look dangling, `L4299-4308`). Dangling names then pay ONE extra
dentry walk collecting the RECORD identities (a collision chain holds several keys for one name, so
the key is the identity, `L4373-4425`).
Findings: `C10NlinkTooHigh{ino}` (the LEAK direction — never reclaimable), `C10NlinkTooLow{ino}`,
`C10ZeroNlinkNamed{ino}` (the DATA-LOSS direction — `destroy_inodes`' live-`nlink` skip no longer
protects an inode a live path resolves; **the operator stops and reads on these two**),
`C10DanglingDentry{vol, key_hex, child_ino}` (`L635-653`). Directories are never a count finding
(`nlink = 2 + subdirs`, synthesized `.`/`..`, `L247-251`).
Zero-FP — NOT by the era floor (a prior-era inode can be hardlinked a microsecond ago, `L227-229`):
(1) **the record witness** `(nlink, ctime)` read under the ino's exclusive 4a BEFORE the fresh pass
and compared AFTER — every same-volume op that changes a name count mutates the record in the SAME
tx, so any change clears the suspect; this kills the pass's read-over-time skew (two dentry pages
either side of one atomic rename look like 0 or 2 names) because the skew can only come from a
commit inside the bracket (`L231-239`, `L5764-5785`, `L6498-6506`); (2) the open cross-volume
intent exemption (`L240-246`, `L6469-6476`); (3) directories excluded; (4) completeness gates
(`L253-257`). Verdict numerator = the DEDUPED names the fresh pass collected; `names == 0` ⇒ C9's,
`nlink == names ∨ (dir ∧ nlink > 0)` ⇒ cleared; direction decided from the verified numbers so
neither arm can drift from the other's ladder (`L6507-6584`). Dangling: exclusive 4a on the PARENT,
the record exists VERBATIM, still names this child, the child still has no record (`L6586-6655`).
Residual stated: a CROSS-volume rename stamps the moved inode's ctime as a separate fragment, so one
could survive one bracket; it must then survive a second full pass and, for the one lowering repair,
a third at repair time (`L259-266`).
**Order vs the block plane**: a zero-count-with-a-name inode's blocks read as C2-leaked (the census
skips `nlink == 0`), so C10 runs BEFORE the block classes and the repair's census is RE-WALKED after
an inode-plane raise (`census_stale`, `L268-276`, `L8371-8374`, `L8455-8470`).
Repair (ONE ladder for all three count arms, `L9237-9434`): intent ⇒ refuse; incomplete dentry pass
⇒ refuse; ONE lease across verify AND mutate (the adjust holds it — a re-acquire would self-deadlock
the non-reentrant stripe lock); directory ⇒ refuse; `names == 0` ⇒ refuse (C9's); equal ⇒ healed;
direction reversed ⇒ refuse ("re-run detection rather than apply a stale verdict"); quarantine the
record; hook; `routed_nlink_adjust(delta)`; a RAISE sets `census_stale`. Verbs
`lower-nlink-to-counted-names` (the one C10 repair that could make a named inode reclaimable —
third independent pass, record unchanged, no open plan, names ≥ 1) and
`raise-nlink-to-counted-names` (the safe direction: an over-count delays reclaim; dropping a name to
match a low count is never the repair) (`L7839-7868`). Dangling → `remove-dangling-dentry` under the
PARENT's 4a: record present verbatim, decodes, still names this child, **`DT_DIR` refuses** (the
removal must also decide the parent's directory nlink), the child still absent; quarantine key +
value; `delete_kind` (`L9443-9589`).
Port: `meta/economy.zig` (the port's nlink arithmetic, `economy.zig:L255` already names "run fsck"
on a parent-side underflow); the `multi` map is a `std.AutoHashMapUnmanaged(u64, u32)` under the
derived entry budget.

### 3.11 C11 — the map plane (`FSCK:L36-51`, `L5033-5198`; recheck `L6662-6838`; repair refuses `L8763-8806`)

Three arms over kind-7 (block-map tree) records, unsharded, report-only (a false quarantine would
hole a LIVE crossing's staged map; the A1 residue sweep is the reclaim path): **(a) orphan map
records** — the owner skip-scan `block_map_owner_scan` (one probe per distinct owner, never per
record) then per owner: `crossing_in_flight` ⇒ `crossing_exempted`; no live inode record OR a head
that is not kvmap-class ⇒ suspect (`L5033-5073`); **(b) empty kvmap head** — nominated by the census
(§2.1), the head claims mapped data while every read resolves to holes; the size-vs-sparse ambiguity
keeps PARTIAL coverage out of scope (`L5174-5198`); **(c) run-foreign shadow** — a POINT on a
DIFFERENT volume strictly inside a covering RUN's span (same-volume points inside runs are the §2 read
law's legal shape; design §12's sanity arm; STRING records record no verdict; record order is key
order so a covering run precedes its points, `L5084-5166`). C9's era floor is structurally
inapplicable (a map record's ino may be years old while its crossing is live on THIS mount,
`L46-48`). Findings: `C11OrphanMapRecords{vol, ino (volume-LOCAL), records}`,
`C11EmptyKvmapHead{vol, ino}`, `C11RunForeignShadow{vol, ino, run_start, idx}` (`L654-676`).
Ladder (§1.3 shape 4): registry → exclusive 4a → registry again → fresh reads: (a) `records == 0`
⇒ the sweep won; live ∧ head kvmap ⇒ the crossing completed (`L6662-6722`); (b) all four legs —
live, sweep-less sentinel, size > 0, zero records (`L6723-6782`); (c) the shadow pair still present
on a fresh scan — a publish between passes re-canonicalizes the span and clears (`L6783-6838`).
Repair: every arm `report-only` → `refused` with its reason (`L7887-7916`, `L8763-8806`).
Port oracle (landed): `PKM.verifyBlockMap` (`records`, `indices`, `run_foreign_shadows`,
`PKM:L782-815`), `PKM.nextOwner` (the skip-scan, `L823-840`), the stats `fsck_map_orphan_records`,
`fsck_map_empty_heads`, `fsck_map_run_foreign_shadows` (`PKM:L165-177`); the (b) arm's nomination
reads `layout.F_MAP_KVMAP ∧ !F_SWEEP_CURSOR ∧ size > 0` (`layout.zig:L205-206`). Test `PKM:L1501`.

### 3.12 C12 — tenant ranges (`FSCK:L52-111`, `L5214-5279`; recheck `L6839-6961`; repair refuses `L8816-8853`)

Reads: the census mapping list — NO new walk: one interval sort per `(vol, offset, incarnation)` (a
bit-13 key names a LIFETIME, so two incarnations of one offset never meet), same-`off` windows
collapsed to the longest, then a sweep for a window starting inside the furthest reach at a DIFFERENT
`off` (`L5244-5278`). Legal classes by construction: the identical-window clone share and the clone
+ passthrough clip (same `off`, shorter `len` = a prefix of the same image); two whole-block
referencers are both `[0, chunk)` and collapse (`L74-84`). Decoded by the class's OWN tolerant decoder
`tenant_window_class` (`L1684-1725`) — NOT the read funnel's (which refuses every violation with EIO,
the read path's contract): the census resolves the BASE and counts the reference, so `bk:garbage:len`
is healthy for C2 and unreadable for every reader — without the Overrun arm fsck would never say so
(`L85-90`). Overrun arms: ≠ 2 components, undecodable `off`/`len`, `off % LBA_GRAIN != 0`,
`len > chunk` (refused before the grain round-up could overflow), `off + pack_slot_len(len) > chunk`.
Findings: `C12Overlap{vol, offset, ino_a, block_idx_a, ino_b, block_idx_b}` (the two referencers
are the identity — the block alone would collapse every pair), `C12Overrun{vol, offset, ino,
block_idx, mapping}` (`L677-701`). Quarantined (`damaged:`) mappings are never windows (`L5217`).
Ladder: Phase A registry + pack-open ledger (an OPEN pack's slots are being minted right now,
`L5687-5713`); Phase C: BOTH layouts re-read under their inos' exclusive 4a held TOGETHER (ONE
canonical `lock_many` acquisition per volume, ascending — a layout publish takes the same lease, so
the pair is a consistent cut; a pair sharing a stripe is one acquisition, never a self-deadlock,
`L8063-8105`), same `(vol, offset, inc)` ∧ still intersecting at different `off` (`L6839-6912`);
Overrun: the SAME string present verbatim (`L6913-6961`). Repair REFUSED: at least one tenant is
wrong and nothing on the volume says which — quarantining both would destroy the right one; an
unreadable window's base IS referenced and restating the window would fabricate a mapping
(`L105-111`, `L8816-8853`). `fsck_tenant_overlap_findings` is the live must-stay-0 tripwire;
`fsck_repair_classC12` is structurally 0.
Port oracle: `layout.Mapping.validate(chunk)` is the window law (`layout.zig:L304-309`) and
`error.MappingRefused` the Overrun verdict — the v4 binding is a 40 B `MappingWire` (`L152-191`), so
"undecodable decoration" collapses to the codec's totality (`error.Corrupt`) and the arm is the
three arithmetic violations; `layout.ceilGrain` (`L267`) = `pack_slot_len`; `pack.OpenPack` /
`Packer.openPack(k)` (`pack.zig:L203`, `L441`) is the pack-open ledger (`PPL.isOpenPack`,
`PPL:L2451`).

### 3.13 C13 — orphan image extents (`FSCK:L60-68`, `L4508-4528`; recheck `L6097-6134`; repair `L8894-8935`)

Reads: `c13_orphan_image_extents()` per meta volume (`BE:L10176-10207`): every extent a declared
region's grant holds CLAIMED that no tree root reaches (`reachable_image_extents` = every tree's
reachable node addresses mapped to extents, `L10148-10160`), under the SMO mutex + the forest's mint
guard so no image is between its claim and its publication (an SMO's successor before the route flip,
a lazy mint's root before the forest names it); a retired image parked on its tail is a pending-free,
not a claim. Empty on a flat volume and an unpartitioned forest; skipped when no grant holds a claim.
The class = the unpublished-root-swap successor, the page's truncated unclaimed remainder, a return
lost between `advance_durable` and the cadence. Finding: `C13OrphanImageExtent{vol, appender,
extent}` (`L702-711`). Ladder: ONE fresh census per volume for the confirm pass under the same
serialization (`L6097-6134`). Repair `return-orphan-image-extent` (`c13_return_orphan`,
`BE:L10219-10261`): verify under the same serialization (claimed ∧ unreached, else `Ok(false)` =
healed/stale refusal), then the appender FREES it exactly as an SMO retires a predecessor — one
`free(extent)` record in ITS ring (checkpoint-class admission) with the extent parked on its tail
gated on that record's seq, so the cadence's `ReturnExtents` clears the bit and the ring's window
never shows an `alloc` outside the record. Nothing is quarantined: no route reaches the image
(`L7949-7961`). `fsck_repair_class_c13`.
Port oracle: `volume_bind.orphanImagesFn` (`L873-900`) judges by CONTENT —
`node.residueStampedBy(img, appender_id)` ⇒ an orphan IMAGE (`dead_appender_orphans`); a blank or
foreign residue ⇒ `dead_appender_unwritten_returned` (0002 row 352: the reference counted a refill
never written as 72 orphan images — the C13 gauge lied by a grant's worth). The port's C13 for a LIVE
appender reuses the same vtable arm over `extent_grant` claimed runs minus the reachable set;
the repair is `alloc_ext.freeRecord` through the appender's ring (`meta/alloc_ext.zig`).

### 3.14 C14 — slot custody conflict ⇒ refuse the mount (`FSCK:L746-760`, `L4691-4725`; recheck `L6237-6282`; repair refuses `L8941-8958`)

Reads: `slot_custody_census(vol0)` (`REC:L957-1060`): every `Live` page's `Live` slot entries, one
attestation per slot at tree 0's `g` — a second `Live` page for the same slot, or tree 0 leasing it to
ANOTHER appender at that `g`, is a conflict; an entry BELOW tree 0's `g` is stale residue, never a
conflict; a `Releasing` entry never counts; one finding per UNORDERED pair per slot (`REC:L989-1028`).
No legal schedule writes it (a grant moves tree 0 before the new lessee's page names the slot; a
release clears the page before tree 0 unleases). Finding: `C14SlotCustodyConflict{vol, slot,
appender_a, appender_b}`. Ladder: ONE fresh census per volume (`L6237-6282`). The MOUNT refuses on it;
the remedy is the operator's attestation `squeezefs appender clear <uri> <id>` (writes the death
record, marks the page `Recovering`; the next mount recovers it). Repair: report-only → refused
(`L7988-8002`, `L8941-8958`). `fsck_slot_custody_conflicts` must stay 0 (`slot_lease_conflicts` is
the live tripwire, `DSM:L712`).
Port oracle (landed): `PLT.settleEntry(tree0, entry, page_owner)` → `.conflict` for a `Live` entry at
tree 0's `g` under another lessee / `.stale_entry` below `g` / `.drop_entry` for `Releasing`
(`PLT:L757-772`); `PLT.liveOnTwoPages(pages)` (`L777-787`). The mount-path gate already refuses on
`.conflict`; fsck's C14 is the same two functions over the appender directory
(`sym/appender_dir.zig`) + `control_records.SlotState`.
Port addition (rung 10 symmetric defects, decision 0003 §AE.4): the census also reads every
volume's `extent_grant` records WHOLE — each appender's chunk family (`extent_grant:{a}:{id}`)
loaded through `grant_record.Family.load` (every chunk's stamp, its head bound to its key, the
effective chunks disjoint; chunks a present move marker stages excluded). A family that does not
assemble is `C14GrantRecordCorrupt{vol, appender}`: report-only (the record is the manager's only
attestation of the grant — no chunk can be told right from wrong, and a rewritten record would name
extents another custodian holds); a marker is legal crash residue (a staged change that never
landed), swept by the manager's next change of that record, never a finding.

### 3.15 C15 — un-recovered appender (`FSCK:L761-775`, `L4691-4725`; recheck `L6283-6332`; repair `L8968-9033`)

Reads: the same census — a `Live` / `Recovering` page whose identity `(node_token, mount_slot)`
volume 0's death ledger (`dead_member:` records) names, with `window_entries > 0` (every dead ring
read ONCE for the count) or slots still leased to it or `is_manager` (`REC:L1030-1055`); a
`Recovering` page the ledger does NOT name is `recovering_unledgered` — a shape no legal schedule
writes; the mount refuses naming `appender clear` (`REC:L536-539`, `L1056-1059`). A set whose volume
0 is unmounted judges C14 alone (`FSCK:L4689-4690`). Finding: `C15UnrecoveredAppender{vol, appender,
node_token, mount_slot, window_entries}`. Ladder: fresh census; a recovery the ledger poll ran since
the nomination clears it (`L6283-6309`). Repair `recover-dead-appender`: ONLINE on the volume's
MANAGER only — `recover_dead_appenders(vol0, ordinal)` (the §5.9 run: preempt the registrant, replay
the window into its slot trees, flush, record every leaf's tail, unlease in tree 0, return the grant,
page `Recovered`); offline REFUSES naming the mount path, which recovers before serving (an offline
probe holds no ring and no lease to recover under, `L8968-9033`). `fsck_repair_class_c15`.
Port oracle (landed): `recovery.Driver.recoverDead(death)` (`recovery.zig:L312`) IS the repair;
`death_ledger.zig` the ledger reader; the census's "window entries" = the dead ring's replay count
(`recovery.zig` step 3). C15 fsck = `(appender_dir pages × death ledger) → Driver`.

### 3.16 C16 — shared-index drift (`FSCK:L712-727`, `L4534-4563`; recheck `L6135-6202`; repair refuses `L8860-8873`)

Reads: per data volume `shared_index_drift(meta, vol_tag)` — the SHARED-flagged references of every
mounted meta volume vs the index home's entries; `FlagWithoutEntry` (a clone that died between
`MarkShared` and `ShareBlock`, or a lost entry — the block's terminal free consults the index and
frees when nothing remains) / `EntryWithoutFlag` (a cloner that died after `ShareBlock` and before
its publish — the home's GC arm drops the entry at the next release). Unsharded like C8. Finding:
`C16SharedIndexDrift{vol_tag, block_idx, owner_ino, block_index, flag_side}`. Ladder: ONE fresh
census per data volume; a drift a clone's steps 1–3 were mid-way through has closed by now
(`L6135-6170`). Report-only: the flag and the index are two durable homes of one fact; restating one
from the other would erase the evidence of which side lied (`L716-719`, `L8860-8873`).
`fsck_shared_index_drift` = 0 on every healthy set.
Port oracle (landed): `shared_index.sharedIndexDrift(flagged_ref_keys, index_keys)` over two sorted
key lists (kinds 6 vs 9, bodies compared), counting `fsck_shared_index_drift` (`L453-488`). fsck
supplies the two sorted lists from its census (kind 6 records with `flags & SHARED`; kind 9 records
at the index home) and calls it twice (nominate, confirm).

### 3.17 C17 — stripe consistency (`FSCK:L728-745`, `L4565-4947`; recheck `L6207-6235`; repair refuses `L8875-8887`)

Reads: the marker census the ONE dentry walk carried (`(vol, local parent, Marker, child)`), folded
into per-directory maps (`c17_maps`, `L4744-4763`: `commit_target`, `migrating`, `entries`); per
striped directory (`commit_target.is_some()`) the shapes (`c17_shapes_of`, `L4790-4853`):
`IncompleteMap` (k < 2, non-contiguous indices, or the commit marker not naming stripe 0 —
`MARKER_TARGET_INDEX`; the router ignores such a map), `MissingStripe` (an entry's ino has no
record), `MisroutedName` (a name in stripe `i` whose `stripe_of(hash54(name, seed), K) != i`),
`UnmigratedName` (a non-marker name in the directory's OWN tree after `migrating` cleared — a
stale-route insert's trace; reads still serve it, the stripe's entry wins by LWW when both hold it),
and over the fold `MultiplyMapped` (a stripe named by ≥ 2 maps). Finding:
`C17StripeInconsistency{dir, stripe, shape label, name}` (`L728-745`). Ladder: `C17Fresh` — ONE
fresh dentry walk and each directory's shapes derived ONCE for the whole confirm pass (Issue 14);
`None` = the census could not complete: no verdict (`L4897-4947`). Report-only (the C8
fabricate-direction posture, `DSM:L620`); `fsck_stripe_findings` must stay 0.
Port oracle (landed): `PST.verifyStripes(views, hasRecord, seed, out)` over `DirView{dir, markers,
names}` producing `Finding{shape, dir, ino}` for all five `C17Shape`s and firing the
`fsck_stripe_findings` tripwire (`PST:L654-701`; test `PST:L1112`). fsck supplies the views from
its dentry pass (markers + user names per directory with a NUL-led name, plus every stripe's names).

## 4. C7 — the data scrub (`FSCK:L7071-7317`; `DVL:L432`)

- **What is verified** (`verify_stored_block`, `L7161-7263`): per referenced mapping, read the
  stored image — a decorated tenant reads `base + rel` for `ceil_4k(len)` and slices to the exact
  `len` (`L7192-7194`, `L7237-7240`); an undecorated whole block reads `block_size` on passthrough,
  else `max_stored_image_len(block_size)` rounded to 4 KiB (transformed images carry a
  self-delimiting frame and may exceed the block — the FIND-RW4-A headroom, `L7195-7208`). Then:
  **encrypted ⇒ AEAD open** (`scrub_aead_verified`); **compressed ⇒ frame decode incl. the bit-31
  raw escape** (`scrub_frame_verified`); **plain ⇒ readability only** — no stored checksum exists, so
  read success is the honest verdict (`scrub_readability_only` is the honesty gauge, OQ-B;
  `L7242-7262`). A short read of a decorated image fails (`L7229-7235`); an unparseable mapping is
  `Skipped` (C2-lost owns it); a backend registered but unhealthy fails honestly (`L7217-7223`).
  Plain blocks are NOT checked against refs or sampled: the census IS the reference check (C2/C3/C8)
  and the scrub is device-bound by design — 16 TiB at ~2 GiB/s ≈ 2.3 h single-worker; the measured
  row read scrub/seq = 0.752 (1.60 vs 2.12 GB/s on a `mbps=2000` null_blk, `CLOSE:L281`).
- **Order and throttle**: device-bound sequential — sort by `(volume, offset)` (`L7084-7086`);
  `SCRUB_BATCH = 8` blocks per duty-cycle unit (`L379`, `L7141-7147`); `job_throttle_sleep(elapsed,
  pct) = elapsed × (100 − pct) / pct`, `None` under 1 ms or at 0/≥100 (`JOBS:L4442-4454`).
  Quarantined (`damaged:`) mappings are never re-scrubbed or re-reported (`L7092-7097`).
- **The ladder** (`reverify_scrub_failure`, `L7269-7317`): online a failure re-resolves the ino's
  CURRENT layout (the mapping must still be present verbatim — inline, indirect or kvmap through the
  census decoder), then re-reads under a VALIDATED pin: `Pinned` ⇒ re-verify; `PinnedUnstable` (an
  in-flight W1 patch — a torn read, not corruption) ⇒ cleared; `Refused` (freed under us) ⇒ cleared.
  Offline a failure is a finding directly. Finding: `C7Scrub{ino, block_idx, mapping}` (`L623-628`).
- **Repair** `quarantine-mapping` (`L10329-10419`): re-verify (online under the pin), quarantine
  what is readable (the verbatim device window), hook, flip to `damaged:` (reads EIO); **the physical
  block stays in place for forensics** — the displaced key is deliberately NOT freed (`L8148-8150`).
  `fsck_repair_classC7`.
- **Scrub fault injector** `SCRUB_READ_FAULT_HOOK(read_off) -> bool` — the G-VL-5(b) "dm-error or
  equivalent" arm at the unit tier (`L1320-1333`, `L7210-7216`).
- **Distribution**: designed distributable (read-only, device-bound); v1 ran it coordinator-local
  (`JobType::wire_executable` matches only `Noop`, `JOBS:L304-306`; `FSCK:L329-335`).
- Port oracle: `data/transform.zig` `processRead` (the frame + AEAD open — charter §6.5), the
  `nvme_dev` read path (`io/`), `layout.Mapping.readWindow` for the exact tenant window
  (`layout.zig:L379`), `PBA` pin with the incarnation word (`Incarnation` + `inflight`) for the
  validated pin; the fault hook becomes a `SQUEEZEFS_TEST_*` seam (§9).

## 5. Sharding `k/N` and `merge-reports`

- **Partition key**: the GLOBAL ino residue `global_ino % n == k` (`FSCK:L3583-3587`); the dentry
  pass filters by the CHILD ino the value carries (`L4004-4008`, the shard rule §2.3); the staging scan
  by the custody key's ino (`L5583-5587`). Deterministic: no coordination, `0 ≤ k < n` parsed from
  `"k/N"` (`MAIN:L2360-2370`). Offline only — online fsck runs whole on the coordinator
  (`MAIN:L2299-2305`). A shard skips the allocator-dependent classes (C2-leaked, C3, C6), C8, C11,
  C12, C13–C17 (`FSCK:L1986-2022`, `L5315-5318`) and emits `partial: PartialCensus{refs, mappings,
  mappings_complete}` (`L1129-1148`, `L1909-1915`). `--repair` is refused on shards
  (`MAIN:L2250-2256`; `FSCK:L10493-10499`).
- **`merge_reports`** (`FSCK:L2196-2307`): findings concatenated then `sort + dedup` by identity
  (per-shard-repeated classes C1/C5 collapse); census counters SUM (each shard judges its own residue);
  per-run gauges take the MAX (`nodes_walked`, `scan_secs`); `inode_plane_covered` is a UNION of
  volume identities, never a sum (`L2229-2237`); partial censuses merge `refs[vol][off] += c` and
  concatenate mappings; an input with no partial marks `mappings_complete = false` (`L2266-2282`);
  `mode` = the last input's; `shard = None`; `findings_elided = 0` (merge inputs are durable
  reports by contract). The CLI exits 1 iff the merged report has findings (`MAIN:L2258-2277`).
  Equivalence pinned: `T/fsck_tests.rs:L1497 test_offline_shards_merge_equivalence`.
- The cross-shard finalize over the merged census (`run_fleet`'s FINALIZE, `FSCK:L2823-2977`) was
  the fleet-wire's; the offline `merge-reports` CLI never consults `mappings` (`L1139-1141`). §11 Q3.

## 6. The report, the gauges, the exit codes

- **`FsckReport`** (`FSCK:L1154-1191`): `schema: u32 = 1` (`L352`), `mode: "online"|"offline"`,
  `shard: Option<"k/N">`, `findings: Vec<FsckFinding{class, object, evidence, identity}>`
  (`L549-565`), `counters: FsckCounters` (`L777-975`), `partial`, `repair: Option<RepairReport>`,
  `inode_plane_covered` (retired, §11 Q1), `findings_elided` (the admin-lane bounded VIEW's count —
  `to_bounded_json(max_bytes)` binary-searches the longest finding prefix that fits a 60 KiB cap; the
  durable report stays complete; elided findings ARE findings for the exit verdict, `L1193-1253`).
  JSON via `--json` (`MAIN:L2398-2404`); the text form prints the census line, the suspects line, the
  C12/scrub lines and `findings: 0 (clean)` or `[class] object — evidence` rows (`MAIN:L2405-2475`).
  Port: snake_case JSON through `std.Io.Writer` (0002 row 202), the finding identity as a tagged
  union rendered `{"kind": "...", ...}`; the persisted job report drops `partial` (its mapping
  identities blew the 64 KiB xattr cap — a 512-block corpus's report was 66 KB, so NOTHING persisted
  and the CLI read "no report", `JOBS:L2233-2256`).
- **Exit codes** (`MAIN:L2272-2277`, `L2351-2354`, `L2391-2394`): 0 clean; **1 when findings exist**
  (including elided) — also for a dry-run `--repair` with findings (`DVL:L441`); argument-lattice
  refusals are errors (`--apply`/`--quarantine-dir` without `--repair`; `--repair` with `--shards`;
  `--offline` on a mountpoint; `--online` on a URI; `--shards` online; trailing files without
  `merge-reports`, `MAIN:L2239-2305`). Port: `cli.exit_ok/exit_failed/exit_usage` (`cli.zig:L43-47`)
  — findings ⇒ `exit_failed`; the lattice ⇒ `exit_usage`.
- **Gauges** (`publish_metrics`, `FSCK:L7335-7441`; `DVL:L565-568`): the `fsck_*` family —
  `inodes_scanned`, `nodes_walked`, `dentry_refs_indexed`, `current_era_exempted`,
  `unreferenced_intent_exempted`, `nlink_names_counted`, `nlink_mismatch_{high,low}`,
  `nlink_zero_named`, `dangling_dentries`, `nlink_transient_cleared`, `inode_plane_{volumes_covered
  (stored, the LAST pass's answer), foreign_scoped, foreign_slot_scoped, window_scoped,
  foreign_dentry_scoped, slots_covered (stored), cross_owner_declined, proposals_admitted,
  proposals_stripped}`, `c1_projection_slots_scoped`, `blocks_checked`, `refcounts_checked`,
  `suspects`, `suspects_cleared`, `epoch_exempted`, `inflight_exempted`, `mover_ledger_exempted`,
  `pack_ledger_exempted`, `alloc_census_declined`, `alloc_bitmap_leak_candidates`,
  `alloc_bitmap_tracked_exempted`, `map_orphan_records`, `map_empty_heads`,
  `map_run_foreign_shadows`, `crossing_exempted`, `tenant_overlap_findings`, `shared_index_drift`,
  `stripe_findings`, `slot_custody_conflicts`, `unrecovered_appenders`, **`findings` (must be 0 on a
  healthy volume — THE tripwire)**, `scan_secs` (stored); the `scrub_*` family `{blocks_scanned,
  bytes_scanned, aead_verified, frame_verified, readability_only, failures}`; the repair family
  `fsck_repairs_{planned,applied,refused}`, `fsck_repair_refused_multi_owner` (retired),
  `fsck_quarantined_{records,blocks,bytes}`, `fsck_repair_class{C1..C10}` + `_c13` + `_c15`
  (`L10428-10447`; `fuse_client.rs:L6524`, `L6555`, `L8543`). Engagement law: a healthy fleet has
  `findings == 0` while `suspects_cleared`, `epoch_exempted`, `inflight_exempted`,
  `current_era_exempted`, `nlink_transient_cleared`, `crossing_exempted` GROW under churn — "its
  growth is the proof the shield is doing work rather than sitting vacuous" (`L790-795`, `L820-826`,
  `L939-942`).
  Port: `counters.ShardedCounter` for every count, `counters.Gauge` for the stored ones,
  `counters.Tripwire` for `fsck_findings`, `fsck_tenant_overlap_findings`, `fsck_stripe_findings`
  (landed, `PST:L87`), `fsck_slot_custody_conflicts`, `meta_kv_block_refs_drift` (landed) and
  `data_alloc_bitmap_drift` (landed) — the per-site label is the class.

### 6.1 The walk's cost and its bound (port; decision 0003 §AC)

Every store read a run makes is tallied (`census.WalkTally`, through the `census.Store` face; the
set counts at the tree — `KvTree.rangeCounted`/`lookupCounted`, `MetaSet.rangeCounted`) and the
report carries it: `nodes_walked` (tree nodes the descents fetched — set at last; the pager used to
bump the global per page and leave the report's field 0), `entries_visited` (entries the store
delivered), `entries_read` (entries the trees copied to deliver them), `store_calls`,
`trees_consulted`, and the wall clock per pass — `census_ms` (inode + layout kinds), `dentries_ms`,
`c1_ms`, `classes_ms` (C2–C17), `recheck_ms`, `scrub_ms` (its fresh census included); all MAX-merged
(every shard walks the whole space). `.stats`: `fsck_nodes_walked`, `fsck_entries_visited`,
`fsck_entries_read` per run.

The bound, two tie-tested functions: `walkReadBound = 3 × visited + trees consulted` (the merged
walk's per-call law summed — `meta/merged_walk.readBound`: a call that fills its `max` consulting
`p` of T trees reads ≤ 2e + p × ⌈max / T⌉, one that ends short reads exactly what it delivers) and
`walkNodeBound = height × (visited + calls)` (node reads per entry within the tree height; a call
that delivers nothing still descends once). An fsck over N entries in D directories is therefore
O(N) entries read and O(N × height / page) node reads for its walks plus O(height) per point probe
(a suspect's re-read; C11's owners come off a walk too, §6.2) — linear, whatever D and however the
names are striped.

Measured (venue, one flat directory, `fsck --offline`): before, the set's merged range read `max`
from EVERY slot tree per page and kept `max` (64 trees × 512) — 29 / 235 / 2,713 / 5,736 / 9,435 ms
at 2 k / 8 k / 32 k / 64 k / 100 k entries, 8.1–9.4 node reads and up to 37 entries read per entry
visited (the cap: T × page past 32 k); after, 8 / 34 / 152 / 319 / 510 ms, 0.18–0.23 node reads and
1.07 entries read per entry visited. The shared-hot-directory row's closing fsck over 52,871
entries in one 64-stripe directory: 83.5 s → 0.79 s. Rows: `fsck/scale.zig` (2 k → 8 k names, flat
and striped, within both bounds, 4N ≤ 4.5 × N on reads and nodes).

### 6.2 The per-suspect lookups and their bound (port; decision 0003 §AC.6)

A class that names something per suspect resolves it through an index built once, never a scan of
a census list per suspect: C2-lost's owner (the derived census carries each block's first owner),
C9's block count (an ino → bindings index of the census's owners, built at the first candidate),
C10's distinct names (one pass over the fresh dentry pass's collected names), C17's routing check
(every decoded map's stripes, sorted, each view looked up once), the ladder's C2-leaked / C3 / C6
re-checks (one derivation of the fresh census per volume). C11's owners come off ONE paged walk of
the map kind (§6.1's pager), where its owner skip-scan probed once per inode. The bound: each lookup's work ≤ 3 × its inputs
(owners or bindings, names, views + mapped stripes + routed names, plus the suspects); C11's store
calls ≤ the entries its walk visits / `SCAN_PAGE` + 2, plus each owner's own reads. The report:
`c2_owner_steps`, `c9_owner_steps`, `c10_name_steps`, `c11_store_calls`, `c17_route_steps`,
`recheck_derived_steps` (SUM-merged), the text form's `lookups:` line. Rows: `fsck/tests.zig`
"… is linear (0003 §AC.6)" (400 fill + 60 damaged each) and `fsck/scale.zig`'s C11 bound.

## 7. Online fsck as a JOB vs offline

- **Online** (`MAIN:L2299-2355`; `IPC:L1519-1575`; `JOBS:L2096-2260`): the CLI submits over the
  admin lane `fsck [scrub|scrub-only] [repair] [apply] [qdir <path>] [throttle <pct>]` → `JobSpec{
  JobType::Fsck{scrub, scrub_only, repair, apply, quarantine_dir}, throttle_pct}` → `job_id`; polls
  `job-status` every 500 ms until `completed` (`failed`/`cancelled`/`paused` are errors — a paused
  detection re-runs whole); reads `fsck-report` (the bounded view) and exits by `has_findings`. The
  executor `run_fsck_job`: `FsckOptions::online()` with the ctl's live throttle; a cancel watcher
  mirrors `ctl.cancelled || ctl.paused` into the engine's flag every 100 ms (pause = cancel; no
  partial-resume state; a stop latch, never an abort — the D0 heartbeat precedent); `run_fleet` (=
  `run` at zero fleet capacity, pinned) → optional `repair` → persist `job:{id}:report` on ino 1
  truncated LOUDLY to the xattr cap with `findings_elided` counted; findings > 0 leave the job
  **Completed** (detection succeeded; the findings are its output — the CLI owns exit codes) but log
  at error (`JOBS:L2090-2095`, `L2213-2219`). Coordinator-LOCAL by design (C1–C6 read live
  RAM-authoritative state no remote client can see, `FSCK:L329-335`).
- **Throttle** = KD-3's per-worker duty cycle `sleep t × (100 − pct) / pct` (charter §8; `JOBS:L4442`),
  live-retunable through `ctl.throttle`; the census pays it per `SCAN_PAGE` (`FSCK:L3651`), the C1
  walks per page (`L3432`), the scrub per `SCRUB_BATCH` (`L7141-7147`).
- **Shard leases / result proposals** (the fleet fan-out, `FSCK:L2398-3041`): `n = read_capacity +
  1` residue shards dispatched to enrolled read workers under 30 s TTL leases with a 10 s heartbeat
  (`JW:L181-183`); the coordinator runs shard 0 ONLINE with suspects armed while members run offline-
  posture over their coherent views (`L2506-2520`); a progress deadline `max(lease TTL, 4 ×
  shard-0 wall, ENROLL_DIAL_TIMEOUT = 10 s)` (`L2523-2541`; `fleet_collect_progress_floor`,
  `L2372-2381`); a lost/refused/mislabelled proposal RE-LEASES to another idle worker else runs
  locally (`job_fleet_shards_relocal`); a stale proposal never merges (`job_remote_refused_stale`);
  `merge_reports` + FINALIZE (C2/C3/C6, C8, C16, C12, C11 over the merged census; the inode plane
  whole from the coordinator's one view) + the shared `recheck_suspects` ladder. §11 Q3 retires the
  wire half and keeps the law.
- **Offline** (`run_offline`, `FSCK:L10457-10477`; `run_offline_body`, `L10536-10637`):
  `format_preflight(path, true)` refuses under a live writer (a crashed holder's `writer_claim` stays
  heartbeat-fresh for the TTL — a contract judging a set right after a kill reaches the census through
  `run_offline_over` with the probe it holds, `L10521-10534`) → probe opens → a probe-shaped router
  with the config's block size and a passthrough crypto (plaintext bound) → every live data volume
  registered → full runs rebuild the allocator census (`recover_active_blocks_v3`, the C2/C3 ground
  truth an unmounted set can offer; shards skip it) → staging dirs = every config root + every
  isolated per-mount dir under `<root>/squeezefs/` except `cache_segment` → `run` → optional repair
  under the D0-GUARDED open (`run_offline_repair`, `L10487-10519`). Port: `cli/offline.zig` already
  opens the set offline (`meta/routing/offline.zig`); the probe posture is `-o ro`'s reader plane
  (`sym/reader_plane.zig`).

## 8. The repair engine (`FSCK:L7443-10447`)

- **Options** `RepairOptions{apply (dry-run DEFAULT), quarantine_dir (default `<first staging
  dir>/quarantine/`; REQUIRED on cache-less filesystems when any action quarantines), multi_owner
  (retired)}` (`L7450-7469`). **Plan = apply's input**: `planned_action(id) -> (verb, detail)` for
  every finding with an identity; dry-run returns the plan and mutates NOTHING (`L8222-8253`).
  Verbs: `quarantine-report-only`, `rebuild-in-place`, `report-only`, `free-leaked-block`,
  `quarantine-mapping`, `verify-content-then-repair-allocator-or-quarantine`,
  `recount-and-set-refcount`, `quarantine-then-discard-custody`, `quarantine-staging-dir`,
  `recompute-accounting`, `release-stale-block-refs`, `destroy-unreferenced-inode`,
  `lower-nlink-to-counted-names`, `raise-nlink-to-counted-names`, `remove-dangling-dentry`,
  `return-orphan-image-extent`, `recover-dead-appender` (`L7740-8019`).
- **Apply order**: the inode plane first, C9 before C10, then everything else, stable (`L8346-8370`);
  `census_stale` after a raise re-walks the census before the next block-class verification
  (`L8371-8374`, `L8455-8470`); ONE fresh census for C2/C3 (`L8286-8298`), ONE repair-time dentry
  pass carrying C9's "is it named" and C10's deduped identities — `None` REFUSES every inode-plane
  action (`L8300-8332`), ONE open-intent read (`L8333-8345`).
- **Verify-before-repair** per class (§3): a healed/stale/moved finding is a `refused` action with
  its reason, counted, never an error (`L8376-8384`, `RepairCounters.refused`, `L7490-7493`).
- **Quarantine** (`Quarantine`, `L7554-7667`): per-run dir `<home>/<unix_ns hex>-<pid>/` created on
  the first quarantined byte; each `put` writes `NNNN_<class>_<i>_<suffix>.bin` files, `fdatasync`s
  each, appends a manifest entry `{class, object, action, note, files[{name, bytes}]}` and rewrites +
  `fdatasync`s `manifest.json` `{schema: 1, created_unix, entries}` BEFORE the action's commit
  mutation — a crash between quarantine and commit always leaves an auditable copy. "Nothing is
  destroyed without a copy" (`DVL:L441`). Counters `quarantined_{records,blocks,bytes}`.
- **Kill-9 convergence**: `REPAIR_ABORT_HOOK("<class>:<object>")` fires AFTER the quarantine is
  durable and BEFORE the commit; `true` aborts the run — the finding is re-detected by the next run
  (`L7524-7552`; test `T/fsck_repair_tests.rs:L1587`). Every action is idempotent (apply twice = apply
  once, `L1535`).
- **Online apply authority** = the coordinator, each action under the object's lease; offline = the
  D0-guarded process, exclusive by construction (`DVL:L441`).
- Port: `fsck/repair.zig` over the class table; the quarantine through `io/uring_fs.zig` (write +
  `fdatasync`; no classical I/O — charter §1.2); the manifest through `std.Io.Writer` JSON.

## 9. Knobs, constants, gauges — with WHY and measured numbers

| name | class | value / range | WHY (reference anchor) |
|---|---|---|---|
| `SQUEEZEFS_FSCK_SETTLE_MS` | A/B lever (logged when non-default) | int `0..MS_MAX`, default 2,000 | the §5.6 step-2 settle window before the re-check (`EK:L379`; `FSCK:L347-350`) |
| `SQUEEZEFS_BLOCK_REFS_VERIFY` | A/B lever (landed) | bool, default 0 | the C8 oracle at MOUNT pays the walk the ledger exists to delete; fsck C8 is unconditional (`P/core/knobs.zig:L219`; `FSCK:L1983-1985`) |
| `SQUEEZEFS_FLEET_JOBS` | retired (§11 Q3) | bool, default on | the fleet fan-out / worker halves (`EK:L351`) |
| `SQUEEZEFS_TEST_FSCK_CENSUS_HOLD` | `SQUEEZEFS_TEST_*` seam | bool | park the census after its FIRST inode page until released, so a contract can mint past the start watermarks (`TEST_CENSUS_HOLD_AFTER_FIRST_PAGE`, `FSCK:L357-377`, `L3536-3545`; `T/fsck_c12_tests.rs:L1179`) |
| `SQUEEZEFS_TEST_FSCK_PRE_REGISTRY_HOOK` / `_SCRUB_FAULT` / `_REPAIR_ABORT` | seams | in-process hooks | the registry TOCTOU boundary (`L1297-1318`), the C7 read-fault injector (`L1320-1333`), the kill-9 window (`L7524-7552`) — in the port these are `std.atomic.Value` seams + a test-only vtable, never knobs |
| `SCAN_PAGE` | constant | 512 records | one tree-range fetch = the throttle unit (`FSCK:L354-356`); the port's `tree` page size |
| `SCRUB_BATCH` | constant | 8 blocks | the scrub's duty-cycle unit (`L379`) |
| `CUSTODY_SCAN_MAX` | constant | 1,000,000 | staging dirs are thousands of files; a bound, not an allocation authority (`L381`) |
| `FSCK_REPORT_SCHEMA` | constant | 1 | the report's forward-only schema (`L352`) |
| `ino_set_byte_budget` | derived | `max(budget/64, 16 MiB)` | §2.6 (`L1430-1443`) |
| `c10_count_entry_budget` | derived | `ino_set_byte_budget / 64` | §2.6 (`L1445-1465`) |
| candidate ceiling | derived | `ino_set_byte_budget / 8` | O(damage) lists bounded; convergent batching (`L4076-4089`) |
| C1 wave | derived | `meta_lanes × 4` | hundreds of slot trees on a two-lane pool wedged the loader's bucket lock (`L1820-1831`) |
| fleet progress floor | derived | `max(TTL 30 s, 4 × shard-0 wall, 10 s dial)` | a shard 0 that finished in µs on an empty set must not read a worker inside its first round trip as wedged (`L2372-2381`, `L2523-2541`) |
| admin wire cap | constant | 60 KiB | the bounded report view (`L1216`) |

Measured evidence rows (zero-FP and floors): the corpse-census correction — 152 FPs on a healthy
fleet after tar-x/rm (`FSCK:L3573-3577`); the 2026-08-17 tarx conviction — 25 C10 loss-shape
findings from a member's time-shifted view, all self-healed (`L436-440`); `sym-storm` — 1,444
removals read as 442 dangling names (`L3903-3906`); the drift-gauge law — 1,147 per-suspect C8 walks
on one leg (`L5822-5828`); `sym-crash` — a released window read as 386-vs-385 C6 (`L5397-5399`);
the 2026-07-31 iteration-loop C6 FP (`L5383-5391`); the leg-13 drain-concurrent C2-lost FP
(`L1349-1353`, `L5895-5905`); PR 13i §4.4aw — a creator outpacing the census kept every page's tail
ahead of the cursor for its whole life (`L3488-3506`); §4.4ba/§4.4bk — joiners' slots bounded at
grant-time words read live blocks as leaked (`BE:L12320-12331`); F-R3 — a joiner's mints read
current-era at every censusing mount (`BE:L12425-12431`). Floors: scan/census 0.677 at 1,001,014
inodes (pre-fix 0.283), scrub/seq 0.752, RAM-warm scan 1,815,738 vs census 1,419,779 inodes/s; the
cargo instrument 1,142,885 census vs 117,518 scan inodes/s (`CLOSE:L76`, `L264-266`, `L280-283`);
C8 ≈ 2.9 ns/ref decode + ≈ 90 ns/ref fold (`FSCK:L1975-1981`).

## 10. Tests (name — one sentence — needs)

`T/fsck_tests.rs`: `test_census_walk_baseline_measured` — the fsck scan rate ≥ ¼ of the bare census
walk over 32,768 inodes (the serial-sum tripwire) (device files). `test_healthy_volume_zero_findings`
— zero findings with every counter engaged. `test_c1_bitflip_node_detected` — a flipped node body is
`C1Torn` (probe reopen). `test_c1_semantic_wrong_tree_record_detected` — a checksum-valid wrong-tree
record is `C1Semantic`. `test_c2_leaked_block_detected`, `test_c2_lost_block_detected`,
`test_c3_wrong_refcount_detected`, `test_c4_orphan_staged_record_detected`,
`test_c5_stale_staging_generation_detected`, `test_c6_census_drift_detected` — one seed per class is
exactly one finding. `test_an_unforgotten_corpses_blocks_are_not_c2_leaked` — an `nlink == 0` record's
blocks feed the census. `test_the_drift_gauge_counts_confirmed_findings_only` — the raw C8 comparison
moves no gauge. `test_c7_aead_corruption_detected` / `_frame_corruption_detected` /
`_read_error_detected` — one seed per verified arm (encrypted/compressed devices; the fault hook).
`test_fp_seed_stalled_units_span_both_epochs_zero_findings` — a stalled flush unit + an R5-parked
write across both epochs ⇒ 0 findings. `test_registry_toctou_publish_deregister_race_no_fp` — the
publish/deregister race at the hook boundary never false-positives. `test_mover_ledger_exempts_
prepublish_refcounts`. `test_offline_shards_merge_equivalence` — k/N shards merged ≡ the unsharded
report. `test_scrub_throttle_stretches_duty_cycle`. `test_report_json_roundtrip_and_merge_dedupe`.
`T/fsck_c9_tests.rs`: `test_ino_bitmap_is_one_bit_per_inode_and_indexes_no_root`;
`test_prior_era_unreferenced_inode_with_blocks_is_one_c9_finding` (remount); `test_healthy_volume_
reports_zero_findings_across_a_remount`; `test_current_era_unreferenced_inode_is_never_reported`;
`test_c9_repair_dry_run_plans_and_mutates_nothing`; `test_c9_repair_applied_frees_blocks_and_
accounting_closes`; `test_sharded_scan_reports_exactly_one_c9_finding_across_the_merge`;
`test_creates_racing_a_scan_produce_no_c9_findings` (multi-thread); `test_c9_counters_and_repair_
class_gauge_move`; `test_ino_set_byte_budget_is_derived_not_tuned` (tie).
`T/fsck_c10_tests.rs`: `test_nlink_above_the_name_count_is_reported_as_the_leak_direction`;
`test_nlink_below_the_name_count_is_reported_and_distinguished`; `test_zero_nlink_with_a_live_name_
is_the_dangerous_class`; `test_raising_a_zero_count_runs_before_the_block_classes_and_saves_the_
blocks`; `test_dangling_dentry_naming_a_missing_inode_is_reported`; `test_healthy_volume_with_
hardlinks_and_dirs_reports_zero_findings`; `test_zero_nlink_without_a_name_is_never_claimed`;
`test_an_open_cross_volume_intent_exempts_the_inode`; `test_c10_repair_dry_run_plans_and_mutates_
nothing`; `test_c10_repair_applied_sets_nlink_to_the_counted_names`; `test_c10_repair_refuses_
ambiguous_evidence`; `test_sharded_scan_reports_exactly_one_c10_finding_across_the_merge`;
`test_c10_counters_and_repair_class_gauge_move`; `test_the_guard_gauge_moves_when_a_guard_clears_a_
suspect`; `test_c10_count_entry_budget_is_derived_not_tuned` (tie); `test_links_and_unlinks_racing_a_
scan_produce_no_findings` (multi-thread).
`T/fsck_c12_tests.rs`: `two_tenants_intersecting_at_different_off_are_one_report_only_finding`;
`overrun_misaligned_and_undecodable_decorations_are_each_reported`; `two_whole_block_referencers_are_
not_a_finding`; `identical_windows_of_a_cloned_promoted_small_file_are_not_a_finding`; `nested_same_
off_windows_are_not_a_finding_but_a_moved_start_is`; `a_tenant_released_between_the_census_and_the_
verify_does_not_fire` (the hook); `an_open_pack_block_never_fires_and_the_sealed_one_does`;
`a_healthy_packed_population_runs_c12_empty_ten_times_under_live_promotion`; `an_online_census_is_
bounded_at_the_ino_watermarks_it_started_with` (the census-hold seam).
`T/fsck_repair_tests.rs`: `test_c1_semantic_repair_rebuild_in_place_x3`; `test_c1_raw_key_of_an_
unknown_kind_is_report_only`; `test_c1_torn_node_quarantine_report_only`; `test_c2_leaked_repair_
free_x3`; `test_c2_lost_verified_content_repairs_allocator_x3`; `test_c2_lost_out_of_range_
quarantines_mapping_x3`; `test_c3_recount_and_set_x3`; `test_c4_orphan_quarantine_then_discard_x3`;
`test_c5_stale_generation_quarantine_move_aside_x3`; `test_c6_drift_recompute_x3`; `test_c7_scrub_
failure_quarantines_mapping_x3`; `test_repair_dry_run_default_mutates_nothing`; `test_repair_refuses_
stale_finding`; `test_repair_idempotent_second_run_plans_zero`; `test_repair_kill9_mid_apply_
convergence` (the abort hook); `test_quarantine_manifest_roundtrip`; `test_repair_metrics_published`.
Symmetric / other suites: `sym_manager_tests::c13_finds_exactly_the_unpublished_root_swaps_
successor_and_repair_settles_it`, `::fsck_c13_reports_plans_and_repairs_the_orphan_image_extent`;
`sym_crash_matrix_tests::a_c14_conflict_is_reported_refused_and_cleared_by_the_attestation`
(kill + `appender clear`), `::a_striped_directory_in_a_dead_lessees_slot_recovers_with_its_map_and_
c17_clean`; `sym_shared_refs_tests::a_cloner_dying_after_mark_shared_leaves_a_harmless_flag_c16_
reports`, `::a_cloner_dying_after_share_block_is_reported_by_c16_and_gcd_at_the_release`;
`sym_dir_stripe_tests::every_planted_c17_shape_is_found_and_nothing_else`; `sym_n_daemon_tests::
the_managers_census_watermarks_never_bound_a_slot_a_joiner_leases`, `::a_members_census_shard_walks_
only_its_own_slot_trees`, `::the_managers_census_takes_no_verdict_over_a_live_joiners_stale_projected_
dentries`, `::a_joiner_minted_orphan_is_a_c9_finding_at_the_offline_census_after_every_writer_left`
(N daemons); `durable_block_refs_tests::fsck_reports_no_durable_reference_drift_on_a_healthy_volume`,
`::fsck_repair_releases_a_stale_durable_reference_offline_and_never_fabricates_one`;
`kvmap_walker_tests::a_healthy_kvmap_volume_passes_fsck_with_zero_findings`, `::a_seeded_orphan_map_
record_is_c11_and_the_registry_shields_it`, `::a_foreign_volume_point_inside_a_run_is_c11_and_a_same_
volume_one_is_legal`, `::an_empty_kvmap_head_with_nonzero_size_is_c11_report_only`;
`small_file_packing_tests::online_fsck_during_live_packing_reports_nothing_ten_times`;
`mw_fleet_jobs_tests::fsck_job_report_persists_without_the_partial_census`; every `fsck_clean`
helper in the crash matrices (charter §1.10: fsck clean after every kill — `sym_convert_tests`,
`sym_cross_owner_tests`, `sym_dir_stripe_tests`, `crash_contract_tests`).
Port: the portable half (ladder state machine, `InoBitmap`, budgets, `merge`, the report codec,
`tenant_window` arithmetic, the C17/C16/C11/C8 oracles' existing tests) runs on every host; the
device/mount/kill rows are Linux (`scripts/venue zig build gate`) and the kill matrix in `smoke`.

## 11. Quirks (decision 0002 rows to add) — and the better design

| # | Reference quirk | Why it is poor | What we do |
|---|---|---|---|
| Q1 | The KD-PV-7/8/16 **owner plane** inside fsck: `owned_volumes`, `multi_owner`, `inode_plane_only`, `INODE_PLANE_SHARD_BASE`, `admit_inode_plane_proposals`, `destroys_under_multi_owner`, `fsck_repair_refused_multi_owner`, `inode_plane_{volumes_covered, proposals_admitted, proposals_stripped, cross_owner_declined}` (`FSCK:L427-485`, `L2440-2481`, `L3065-3122`, `L7694-7701`, `L8408-8454`) | A retired design (per-volume claim admission, K ≤ 16 owners — `DSM:L56`, `L85`) overlaid on the forest; the forest's LESSEE shard is one grain finer and subsumes it (`DSM:L524`, `L708`) | The lessee shard only: candidates scoped by slot lease + foreign window + era floor + tree 0 (§2.4–§2.5); the destructive trio's online posture is by LEASE — a slot this mount leases is authoritative, so no "report-only under multi-owner" split exists; `fsck_inode_plane_slots_covered ≡ leased ∪ unleased` is the one coverage gauge |
| Q2 | Findings carry `class: String` + `object: String` + `evidence: String` BESIDE the typed `identity: Option<FindingId>`; `None` for old binaries (`FSCK:L549-565`) | Three redundant homes; a `String` class dispatched by `match f.class.as_str()`; an optional identity that repair must refuse | ONE tagged union `Finding` whose tag IS the class and whose payload IS the identity; `evidence` is rendered from it by `format(*Writer)` — never parsed; no `None` arm (forward-only schema) |
| Q3 | The fleet fan-out wire inside fsck: `run_fleet`, `FleetDispatch`, `PartialCensus.mappings`, `staging_full`, `assume_latched`, the collect loop + relocal + FINALIZE (`FSCK:L2310-3041`); `SQUEEZEFS_FLEET_JOBS` (`EK:L351`) | 700 lines of fsck re-implementing the job fabric's shard leases; the inode plane's one-view law meant members contributed only census residues; a member's census is a projection whose staleness the census cannot bound (`L3896-3932`) | fsck is ONE job type on the fabric; distribution is the FABRIC's (shard leases, proposals, fencing — charter §8); the offline `--shards k/N` + `merge-reports` degenerate is kept verbatim; the `-o ro` shard law (charter §5.5) is the member's contract if a fleet row is ever funded |
| Q4 | Block identities as `vol: String` + `offset: u64` + `mapping: String` (`MappingRef`, `FSCK:L1343-1359`), with `canonical_backend` aliasing `backend_0`/`squeezefs` (`L3197-3215`) and `clean_key` string stripping (`L3776-3788`) | String parsing in the census and the ladder (0002 row 105) | `block_refs.Binding{block_index, vol_tag, block_idx}` + the layout's `MappingWire`; the finding carries `vol_tag: u64`, `block_idx: u64`, `(ino, block_index)` |
| Q5 | `damaged:` as a STRING prefix on the mapping (`RT:L1990`, `L2032`) | A string marker in a binary record | `layout.M_DAMAGED` flag bit (landed, `layout.zig:L213-214`); `error.MappingDamaged` at the one read choke point |
| Q6 | C4 custody keys as `active_block[_ext]:inode_{ino}:block_{b}` strings parsed by `custody_key_ino` (`FSCK:L5513-5520`); C5's generation as a text marker `v3:<hex>|…` (`L1266-1291`, `NV:L15`, `L169-182`) | Text identities for binary state | `staging.StagingKey{file_id, ino}` (landed) and the 72 B checksummed `Marker` with `bindVerdict` (landed); C5 = `.dead`/`.foreign_scope` |
| Q7 | The census decodes JSON-or-bincode layouts per ino (`bytes.starts_with(b"{")`, `FSCK:L3620-3624`, repeated five times) | Two serialization libraries as on-disk formats (0002 row 2) | `layout.Layout.decode` — one canonical codec |
| Q8 | `DVL:L445`'s `--drop-damaged` for a torn C1 node is specified but never built; the repair is `quarantine-report-only` with an EMPTY parts list (`FSCK:L9643-9644`) — "quarantine" of nothing | A manifest entry that copies no bytes; a spec row without code | Keep the honest action (report, the finding persists); the manifest entry records the cursor + error and the SUBTREE's key range, not an empty copy; `--drop-damaged` is not built (no fabrication, no silent ENOENT subtree) until a measured need |
| Q9 | `fsck_end_scan` DRAINS the side map (`BA:L1393-1396`, `scc::HashMap::clear_sync`) and the hot path takes a hash-map entry per mint while armed (`BA:L2503-2505`) | A concurrent hash map on the allocation path under a scan; a drain that races a late reader | The port's per-cell epoch word (landed, `PBA:L728`) and epoch COMPARISON (`>= scan_epoch`), no drain (§1.4) |
| Q10 | C13's orphan census judges by REACHABILITY alone (`BE:L10197-10206`) | Counted a never-written refill as orphan images (0002 row 352) | Content-judged (`node.residueStampedBy`), landed in the dead-appender arm; the live-appender arm reuses it |
| Q11 | The repair-class gauges as a fixed `[AtomicU64; 10]` indexed by `class_idx` plus two loose `_c13`/`_c15` words, exported `fsck_repair_classC1` (CamelCase) (`fuse_client.rs:L6524`, `L6555`, `L8543`, `L11707`) | A hand-indexed array that cannot name C11–C17; a casing exception (0002 row 202) | One `ShardedCounter` per class in a `comptime` table over the finding union's tags, exported `fsck_repair_class_c<N>` |
| Q12 | The online CLI polls `job-status` every 500 ms and exits by a string `state` match (`MAIN:L2330-2346`) | String-parsed outcomes deciding a verdict (charter §12) | The job record's state is a typed enum on the admin wire; the CLI waits on the fabric's completion |
| Q13 | `inode_plane_slot_coverage` is read per volume by both the C1 unit builder AND the dentry pass (`FSCK:L3281`, `L3918`) — two reads of one predicate per run | Two instants of one truth | ONE `Coverage` snapshot at pass start, shared by every walker |

## 12. Port plan — each class onto its oracle; sizes (lines of new Zig, tests included)

New module tree `src/fsck/` (portable unless noted):

1. **`fsck/report.zig`** (~350): `Finding` tagged union (one tag per class × arm, payload = identity),
   `Counters` (the §6 families as plain `u64` fields folded into the global `ShardedCounter`s at
   publish), `Report{schema = 1, mode, shard, findings, counters, partial, repair}`, canonical JSON
   writer/reader (`std.Io.Writer`; snake_case), `merge(reports)` (§5 — sort + dedup by identity,
   sums, maxes), `boundedJson(max_bytes)` (binary search), the text renderer. Tests: round-trip,
   merge dedupe/sum/max, bounded view keeps the exit verdict.
2. **`fsck/ino_bitmap.zig`** (~180): §2.2 over `inos.routeIno/makeGlobalIno`; the byte budget.
   Tests: 1 bit per ino, root never indexed, ceiling refusal, word-parallel difference, truncation.
3. **`fsck/budget.zig`** (~60): `inoSetByteBudget(facts)`, `c10CountEntryBudget`, the candidate
   ceiling, the C1 wave — pure `derive.Facts` functions with tie tests.
4. **`fsck/census.zig`** (Linux; ~550): the ONE inode-major walk over every judged slot tree
   (§2.1) dispatching per kind — wraps `PPL.derivedWalk`'s layout half, `PKM.deriveBindings` for
   tree heads, builds `live`, `odd_nlink`, `referenced` (+ `multi`, collected `names`, C17 markers),
   the kind-6 shared-flag list and the kind-9 index list (C16), the C11 (b) nominations; bounded at
   the start watermarks (`inos.InoWatermark.snapshot` per keyspace + the lease table's own leases +
   tree 0's unleased words; foreign-leased ⇒ walked whole); `Coverage` snapshot (Q13) from
   `lease_table` + the membership owner's live set; the census-hold test seam.
5. **`fsck/era.zig`** (~80): `priorEra(ino) -> {prior, current, unknown}` (§2.4) from the open
   watermarks + `slot_state` words; tests for the three answers and the tree-0 witness.
6. **`fsck/ladder.zig`** (Linux; ~450): `Suspect` union mirroring `Finding`; Phase A (epoch →
   registry → mover ledger → pack-open ledger over `PBA.allocationEpochOf/inflightContains` +
   `PPL.isOpenPack`), the settle (`io.sleep` through the executor's `Io`; `PBA.fsckBumpEpoch`),
   Phase B (one fresh census; the C10 witness bracket under `dlm.lockMany` exclusive), Phase C per
   class with the memoized fresh censuses (C8 via `PBR.verifyRefs` → `PBR.confirm`; C13 via the
   content-judged census; C16 via `shared_index.sharedIndexDrift`; C17 via `PST.verifyStripes`;
   C14/C15 via `PLT.settleEntry`/`liveOnTwoPages` + the death ledger). Model: the Phase-A order is a
   plain state machine, explored with `model/explorer.zig` against the publish/deregister race
   (the TOCTOU weakening = reference-state-first must FAIL).
7. **`fsck/classes/*.zig`** (Linux; ~900 total): `c1.zig` (raw slot-tree page walk + ordering +
   `key`/`record` schema; the raw-key deletion gate over `key.classifyRefused`), `c2_c3_c6.zig`
   (over `PBA` + `data_bitmap.verifyBitmap` + the lost arm's bitmap exemption), `c4_c5.zig` (over
   `staging.Segment.residents` + `Marker`/`bindVerdict`), `c9_c10.zig` (the differences, the gates,
   the dangling identity pass), `c11.zig` (over `PKM.nextOwner/verifyBlockMap` + the crossing
   registry), `c12.zig` (over `layout.Mapping.validate`; the interval sweep per `(vol_tag, block_idx,
   incarnation)`), `c13_c15.zig` (thin: the recovery vtable + `Driver.recoverDead`), `c16_c17.zig`
   (thin adapters).
8. **`fsck/scrub.zig`** (Linux; ~250): §4 over `transform.processRead` + the uring read path +
   `layout.Mapping.readWindow`; the validated pin; the fault seam; `SCRUB_BATCH` throttle.
9. **`fsck/repair.zig`** (Linux; ~900): §8 — the plan table, the apply order, `Quarantine` over
   `uring_fs`, the abort seam, every class's verify-before-repair arm (C8 through
   `PBR.repairPlan/stageRepair` offline-only; C13 through the appender's ring free; C15 through the
   recovery driver online-only; the report-only classes refuse with their reasons).
10. **`fsck/engine.zig`** (Linux; ~300): `run(ctx, opts)` — pass 1 concurrency (unthrottled fan-out
    in waves on the meta pool; serial under throttle), the class order, the settle, publish; the
    offline harness (probe open, allocator census rebuild, staging roots) and the guarded repair open.
11. **`cli/fsck.zig`** (Linux; ~250) + `cli/args.zig` rows: `fsck <target> [--online|--offline]
    [--json] [--throttle pct] [--scrub] [--shards k/N] [--repair [--apply] [--quarantine-dir d]]`,
    `scrub` (= `--scrub-only`), `fsck merge-reports <files…>`; the argument lattice (§6) as usage
    refusals; exit 1 on findings. The online arm submits the fabric job and waits on its typed
    completion (Q12); the report persisted beside the job record without `partial`.
12. **The job fabric's fsck face** (rung 8's fabric module; fsck needs only: `JobType.fsck{scrub,
    scrub_only, repair, apply, quarantine_dir}`, `throttle_pct` live-retunable, a cancel/pause flag,
    the `job:{id}:report` record on ino 1 behind the reserved-xattr screen, and the per-worker
    duty-cycle sleep `elapsed × (100 − pct) / pct` floored at 1 ms).
13. **Stats + rails**: the `fsck_*`/`scrub_*`/`fsck_repair_*` families registered in
    `stats/stats.zig` (one `Family`), every knob of §9 a registry row in `core/knobs.zig` read by
    product code (`tests/rails.zig`'s inverse census), the `SQUEEZEFS_TEST_FSCK_*` seams classed
    `test`.
14. **Evidence note** `docs/evidence/rung-08-fsck.md`: the zero-FP rows (ten online runs under
    churn + the kill matrix's `fsck clean` on every row), the scan/census and scrub/seq floors on the
    venue's null_blk, the model's weakening, and the 0002 rows Q1–Q13 added.

Total ≈ 4,500 lines of new Zig (≈ 1,400 of it tests), against the reference's 10,637 + its
backend helpers — the difference is the oracles already landed, the typed finding union, and the
fleet wire retired to the fabric.
