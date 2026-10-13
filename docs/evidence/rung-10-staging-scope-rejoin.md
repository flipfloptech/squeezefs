# Rung 10 — the staging ring's member scope, a joiner's rejoin, and the shipped free (2026-10-12)

The jobs agent stated two defects while building the fleet READ shard row (`rung-10-jobs.md`
§4): two mounts of one set on one host shared ONE live staging ring, and a joiner's clean
unmount + rejoin with the manager up was refused. The rejoin row's staged files then exposed a
third, worse one: a joiner's cleanly promoted staged files were destroyed by the manager. Design
calls: `0003` §T; reference departures: `0002` rows 500–501. Landing 1 carried §1–§3 (defects B and C);
landing 2 carries §4 (defect A). Venue: `sqz-zig` (Ubuntu 24.04, kernel
7.2.6-sqz, 9 vCPU / 16 GiB), the two-daemon rig of `src/cli/smoke.zig` and a pid-tagged shell
repro (A = manager, B = joiner over TCP on one host, one cache path).

## 1. A joiner's clean unmount + rejoin (defect B)

**Repro (dev 37cddb2, ReleaseSafe).** A mounts, B joins and writes 100 files, `squeezefs umount`
B (clean: 64 slots released at g 1, the page Free), B mounts again at the same mount point: 4 of
5 attempts refused `cannot open metadata set …: corruption outside the replay window`, with no
error line in B's log naming a cause.

**Root cause.** The error-return trace of the open: the JOINED backend's first checkpoint →
`publishGuestRoots` → `slots.Plane.publishRegionBeforeTail` → `publishTree0(slot)` →
`PublishRoots` over the wire → the manager's screen refused (`slot 115: cur unleased appender 0
g 1 vs req appender 1 g 1`) → `Corrupt`. B's rejoined incarnation believed it held slots of its
PREVIOUS incarnation: the manager's `appender_hint` hands a rejoin the same appender id (1), and
`armJoined` loads tree 0's PROJECTION — the manager's last ledger record plus whatever tree-0
leaves an SMO had carried onto the device since — into the lease table as-is. Rows `Leased{1,
g 1}` whose releases the manager had journaled but not yet recorded loaded as HELD by the new
appender 1 (the diagnostic run: the projection held `{107 … 128}` as ours at `mounted seq 4`,
the manager's record unchanged between B's two opens). The page plan then published their roots
and the manager refused. Had no publication been due, the door would have admitted writes into
slot trees this mount did not lease — the partition law broken silently. The race window is B's
remount inside A's 1 s checkpoint cadence, hence "most times".

**The law (0003 §T.2).** The joined open refuses a `Live` page under its identity
(`PredecessorLive`), so a rejoin is always a fresh incarnation holding exactly the manager's wire
grants. `lease_table.joinedProjectionRow` loads every projection row naming the joiner's OWN
appender id `unleased` (the words kept as the cold floor, the holder cache never learns it); a
first touch of such a slot asks the manager, the live truth. Counted
`slot_projection_stale_self_rows`, logged at info.

**Red rows.**
- `meta/sym/wire_plane.zig` "linux: A JOINER'S CLEAN UNMOUNT AND REJOIN while the manager stays
  up …" — a TestSet manager with its wire plane up, B a `MetaSet.open(.join)` in-process over
  TCP on 127.0.0.1. The race made deterministic: the manager's cadence held
  (`SQUEEZEFS_CHECKPOINT_MAX_AGE_MS=600000`) and ONE rotor slot (`SQUEEZEFS_SYM_MINT_SLOTS=1`: one
  release, no tree-0 split carries the released row onto the device). The precondition is
  asserted (a reader open reads the slot `Leased{appender 1}` on the device while the manager's
  table says `Unleased`). Before the fix the second open answered `error.Corrupt`; after, the
  rejoin holds exactly its new grant, the stale row reads unleased, the counter moves by 1, and a
  record committed in the new slot survives the next cycle. 10/10 green.
- `meta/sym/slots/lease_table.zig` "the joined projection law …" (portable): a leased or
  releasing row naming the joiner demotes, words kept; another lessee's row and an unleased row
  are verbatim; loaded, the joiner holds nothing.

**Smoke row** (`smoke: A JOINER'S CLEAN UNMOUNT AND REJOIN while the manager stays up`): A up;
B joins and writes 49 files per cycle (40 × 1 KiB inline, 8 × 64 KiB staged, one fsync'd 8 MiB
striped, cut from the rig's random image); three cycles of `umount` B (clean exit) → A reads
every file of every cycle sha-equal → B mounts again (`JOINED as appender 1` in the new log
segment) → B reads every file sha-equal → B writes the next cycle; then B's last unmount, A
reads all 196 files sha-equal, A unmounts, `fsck --offline --scrub` 0 findings (201 inodes, 16
blocks). **10/10 green**; the race was hit in 7 of the 10 runs (16, 39, 17, 8, 14, 10 and 27 stale
rows demoted across a run's three rejoins) — each a refused mount before the fix. Repro after the
fix: 5/5 rejoins mount.

## 2. A joiner's promoted staged files destroyed by the manager (defect C, found by the row)

**Repro.** A up, B joins, B writes four 64 KiB files (staged in B's ring), `umount` B: "dismount
promotion: 4 staged file(s) (278528 B) promoted, 0 failed" — then every one of the four files
read EIO on A, ZEROS on B's rejoin and on A's remount; `fsck --offline --scrub` clean (6 inodes,
4 tenant windows readable). A's `.stats` across B's unmount: `terminal_frees` 0 → 2,
`block_free_file_punches` 0 → 2, `block_free_punch_bytes` 8,388,608, `free_seeded_references` 2.
The same repro with a per-process staging root (a scratch hack) failed identically — not the
shared ring.

**Root cause.** B's dismount sealed its two open packs; the seal releases the pack's pin
(refcount `committed + 1` → `committed`, nonterminal by the pin arithmetic, no
`pack_pin_refcount_drift`). On a non-holder `Reclaimer.freeBlock` SHIPPED every release to the
allocation holder BEFORE its own ladder ran; the holder's executor found the block untracked
(nothing minted it there) with durable population 0 — it cannot read the references a LIVE
lessee's slot trees carry — and its Row 7 ("a former lessee's mint") seeded ONE reference and
freed it: two pack blocks punched under four live tenants. The reference has the same shape
(`free_block_verdict` ships before the ladder; 0002 row 500). The same path frees a joiner's
pack at the holder whenever one of its tenants is released while siblings live.

**The law (0003 §T.3).** Charter §5.5: TERMINAL frees of a non-holder ship. The non-holder now
releases LOCALLY while its own RAM count covers another reference
(`BlockAllocator.releaseUnlessLast`: CAS down from ≥ 2; a SHARED block excluded — its
terminality is the share home's); only the last reference, a SHARED block and an untracked one
ship, and the shipper's verdict seams retire or release the local word as before. A stale-high
local count can only leak (fsck C2 reconciles), never free a referenced block.

**Red row.** `data/reclaim.zig` "a NON-holder ships only its TERMINAL frees …": a block with three
references on a non-holder reclaimer — two releases shipped 2 blocks before the fix; after, both
stay local (refcount 1, allocated, nothing shipped) and the last ships. The repro after the fix:
all four files sha-equal on A after B's unmount, on B's rejoin, with B up, after B's second
unmount and after A's remount; fsck clean.

## 3. Gates (landing 1: §1 + §2, on dev 900caad)

- Linux (venue, `zig build gate`): **46/46 steps; 2,711 / 2,774 tests passed, 63 skipped**; unit
  2,480 pass / 61 skip; smoke 80 pass / 2 skip (the rejoin row and the online fsck row among them).
- macOS (`zig build gate`): **28/28 steps; 2,142 / 2,168 passed, 26 skipped**.
- `smoke: ONLINE FSCK THROUGH THE ADMIN LANE` failed 2 runs in 3 on pristine dev 37cddb2 (attributed
  in a detached worktree) on its own stated gap: the set stage's C9 over a joiner's cross-owner mint
  (`rung-10-jobs.md` §4) fires the coordinator's `fsck_findings` tripwire site and the row asserted
  A's `invariant_tripwires == 0`. The row now requires A's total to equal that site's count (B's
  total stays 0); the gap itself stays its owner's.

## 4. The staging ring's member scope (defect A — data safety; landing 2)

**Repro (dev).** The two-daemon rig's daemons share ONE cache path (cache paths are the set's
format decision). `openStaging` bound the path itself as the root with a marker written
`node_token = 0, mount_slot = 0` and judged with `bindVerdict(found, ours, peer_live = false)`:
every mount of the set on the host bound the SAME ring. The jobs agent saw B adopt A's 200 live
residents at its open and promote them at ITS dismount through slots it did not hold (197
refused, `pack_pin_refcount_drift`) — two writers mapping one segment file under two placement
indexes. Two more faces of the same scope hole, measured here:
- a SECOND SET formatted on the same cache path: its mount read A's marker as another set's
  (`dead`), unlinked A's live segment and stamped its own; A ran on an unlinked mapping. `.repro`
  on dev: A stages 200 × 64 KiB files, the second set mounts (rc 0), `kill -9` A, remount A:
  "recovered 0 record(s)", **200 of 200** of A's acked files read wrong. The control (no second
  set): 200 recovered, 0 wrong.
- the NVMe read tier's `cache_segment/` shards were punched (cold-wiped) by every co-located open
  under a live peer; `staging list` ran a WRITING recovery scan (tombstones, punches) over any ring
  it found, a live mount's included; a `-o ro` reader bound (and restamped) the writer's root.

**Red first (`data/staging.zig` "linux: the member scope (0003 §T.1) …").** The binding was
first extracted into `staging.Ring.bindMember/bindRoot` with today's law and the row run red: B's
bind returned A's root (same path) and B's recovery scan adopted **3 of A's 3** residents; a
second claim on A's live root was not refused. The row, green after the law: A (manager, slot 0)
stages 3 records; B (joiner slot `0x80001234`) binds its own root and recovers 0; B forced onto
A's LIVE root and A's own member presented by a second incarnation are both refused `OwnerLive`
with the refusal naming A (marker equal to A's: appender 0, pid 1111; `staging_owner_live_refusals`
+2; `/proc/locks` shows the claim); A dies (its claim released, the residents stay): the census
from B's side reports A's root as ownerless residue (1); B on A's dead root is refused
`ForeignOwner` (never adopted, never wiped; `staging_foreign_scope_refusals` +1); A's successor
(same slot, a new node token and pid) adopts all 3 (`match`, the marker restamped to the
successor).

**The law (0003 §T.1; 0002 row 501).** Every mount binds its OWN root under the cache path,
`<cache path>/member-<mount slot as 8 hex>` — marker, staging segment and NVMe read-tier shards.
The member scope is the mount slot the mount presents on meta volume 0 (manager 0, a joiner's
mount-point derivation, a `--single-writer` D0 winner 0), stable across incarnations (the node
token is boot-scoped: a reboot's remount at the same mount point is the successor). The LIVE
CLAIM is an exclusive `flock` on the root directory taken BEFORE the marker is read and held for
the ring's life (the kernel drops it at the process's death). Marker v3 (80 B, `extern struct`,
little-endian, checksummed; the existing marker fuzz row covers the decoder) names the owner:
`(node, mount slot)`, appender id, pid. Under the claim: absent ⇒ fresh; our set and member ⇒
adopt (the predecessor's residue is the successor's); another member ⇒ `StagingForeignOwner`
(never adopted, never wiped); another set / newer epoch / unreadable marker ⇒ wiped (a cache path
is one set's). A held claim ⇒ `StagingOwnerLive`, logged with the owner and the lever: a second
mount of the member (unmount it, or mount elsewhere) or ANOTHER SET on this cache path (`config
set-cache-paths`). Other members' ownerless roots holding ring bytes are reported loud at every
bind (`staging_member_residue_roots`), never touched. A `-o ro` reader binds no root. The online
fsck's C4/C5 face reads the plane's own member root and bound marker; the KD-8 offline rebind
walks every member root keeping its owner and refuses a live one; `staging list` lists member
roots with their owner and claim, never scans a live ring and scans a dead one through a PRIVATE
read-only mapping; `config set-cache-paths` recognises a cache path holding member roots as ours.

**Smoke rows.**
- `smoke: TWO MOUNTS OF ONE SET ON ONE HOST, one cache path …` (new): A stages 200 × 64 KiB files
  (17,203,200 B charged in `member-00000000`); B joins beside A's LIVE ring — its bring-up logs
  `staging ring …/member-<B's slot>/staging_segment/seg-0: … recovered 0 record(s)`,
  `staging_recovery_records` 0, `staging_member_residue_roots` 0, A's charged bytes unchanged;
  `staging list --json` shows both roots bound, both `owner_live: true`; a SECOND SET formatted on
  the same cache path is refused at its `mount --daemon` (exit 1, `StagingOwnerLive`; its log:
  "held LIVE by a mount of ANOTHER SET on this cache path"), A's ring intact, 0 discards; B's
  dismount promotes its 8 files (0 failed) with A's charged bytes unchanged and A reads them
  sha-equal; A's dismount promotes its 200; `fsck --offline --scrub` 0 findings (211 inodes);
  after a remount every one of the 208 files is sha-equal.
- `smoke: ONLINE FSCK THROUGH THE ADMIN LANE`: the jobs agent's workaround (A remounted before B
  joined, so A's residents were promoted away) is gone — B joins beside A's live residents (the
  fill's 200 packed files, still staged) and asserts `staging_recovery_records` 0 and A's charged
  bytes unchanged.
- The `.repro` on the fixed binary: the second set's mount refused (rc 1, `StagingOwnerLive`);
  `kill -9` A, remount: "recovered 200 record(s)", 0 of 200 wrong.

## 5. Gates (landing 2: §4, on dev a72b9f2)

- Linux (venue, `zig build gate`): 43/46 steps; 2,710 / 2,777 tests passed, 64 skipped, 3 failed —
  the three `mem.budget` fleet-share rows (`expected 13743895344, found 6871947672`: the census of
  the ONE container cgroup counted another agent's `squeezefs-unit` run beside ours, the documented
  venue artifact). Re-run in a quiet window (no foreign `squeezefs-unit` in `cgroup.procs`): every
  `mem.budget` row green, 78/78. The rest of unit 2,478 pass / 62 skip; smoke **81 pass / 2 skip**
  (the member-scope row, the online fsck row with B beside A's live residents, the rejoin row).
- macOS (`zig build gate`): **28/28 steps; 2,126 / 2,153 passed, 27 skipped**.

## 6. Owed (stated)

- A dead member's residue whose member scope never mounts again on this host (a joiner moved to
  another mount point) is reported loud at every co-located bind (`staging_member_residue_roots`)
  and never touched; an operator verb adopting it as a named successor (the reference's `staging
  adopt`) is owed.
- A joiner's member scope is 31 bits of its mount point's hash: two co-located joiners whose mount
  points collide are refused while both live (`StagingOwnerLive`, beside membership's OQ-5
  detection); a DEAD collider's residue would be adopted by the other — stated, not engineered.
- Rings of the pre-§T.1 layout (a marker and `staging_segment/` directly at the cache path) are not
  read by this binary — a pre-release format no Zig release carried; promote them (clean unmount)
  with the previous binary before upgrading.
- The online fsck row's stated gap (C9 over a joiner's cross-owner mint) stays the jobs owner's;
  the row only scopes the tripwire site it fires.
