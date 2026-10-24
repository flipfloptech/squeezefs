# Porting spec — Symmetric metadata, the coherence half: GPFS-strict read tokens, cross-owner intents, the record-level ship, directory striping

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

Source of truth (read-only): `/Users/joberdorf/Source/squeezefs` (abbrev. `R/`).
Target: `/Users/joberdorf/Source/squeezefs-zig` (abbrev. `Z/`), charter §5.5 ("Tokens — GPFS-strict",
"Cross-owner namespace ops", "Record-level mutations of a foreign-slot file", "Directory striping"),
§2 lock order, §4 notify (sideband), decisions 0002–0003, `AGENTS.md`.
Law: decision 0002 (the charter's mechanisms, invariants and derivations are requirements; the
reference's incidental choices are not; no compatibility owed) and 0003 (format v4: one key layout,
`extern struct` + canonical total codecs, every magic ASCII, no serialization-library wire).

Reference files: `R/docs/design-symmetric-metadata.md` (1136: §5.1.5 :310–320 the lock/token
manager, §5.1.6 :322–324 slot→holder resolution, §5.6 :566–605 cross-owner ops, §5.6.4 :607–609 the
set-wide lock, §5.6.5 :611–623 striping, §5.7 :625–660 tokens, §5.10 :744–770 the D1 arithmetic,
§6.1 :809–825 knobs, §6.3 :842–865 wire, §11 :991–1011 gauges + closures), `R/src/token_grant_core.rs`
(161, the GRANT ∥ PASS gate), `R/src/token_cache_core.rs` (151, the S8 fencing-token cache words),
`R/loom-models/src/lib.rs` (:7513–7654 `token_grant_models`; :5683 `token_cache_models`),
`R/src/meta_ship/token_plane.rs` (4105: the token wire, holder, reader, mount sink, stats),
`R/src/meta_ship/tokens.rs` (2498: `RecallLane`, the deadline/batch derivations, the records budget),
`R/src/meta_backend/kv/backend.rs` (:3716–4150 the divert, :4348–4455 grant records + the pass hook,
:22979–23055 the hook's site, :27950–28246 the witness + the ONE applier), `R/src/ro_coherence.rs`
(:200–523 `ServeStamp` + the drain), `R/src/meta_backend/kv/revalidate.rs` (:358–524 the predicted-slot
poll, the gap belt, the quiescent reader), `R/src/meta_backend/crossvol_tx.rs` (3644, read whole),
`R/src/meta_backend/mod.rs` (:1812–1856 link, :1892–2074 `xv_serve_step`, :2391–2453 the ancestry walk,
:4319–4403 foreign create, :4846–4951 rename plan, :5070–5184 the rename wrapper, :5552–5618 unlink),
`R/src/meta_backend/kv/backend/crossvol_arms.rs` (:1–480), `R/src/meta_backend/kv/slot_state.rs`
(:955–1021 `dir_rename`), `R/src/meta_backend/record_ship.rs` (544, whole), `R/src/fuse_client.rs`
(:17219–17262 the sinks, :31345–31419 the kernel hook), `R/src/meta_backend/dir_stripe.rs` (2673, read
whole), `R/src/fsck.rs` (:4566–4631 C17), `R/src/meta_ship/wire.rs` (:201–232, :342–470 the verbs),
`R/src/error.rs` (:12–71 `RefusalClass`), `R/src/slot_lease_core.rs` (:1238–1336), `R/src/
slot_holder_cache.rs` (:1–28), `R/src/meta_backend/atomicity.rs` (145 — the sector-atomicity PROBE; it
is not part of this surface and is listed only because the brief named it), `R/.benchmarks/
2026-09-15-sym-pr5-tokens.md`, `…-pr6-cross-owner.md`, `2026-09-16-sym-pr7b-dir-stripes.md`.

---------------------------------------------------------------------------
## 0. Where the port stands (what exists, what this spec adds)
---------------------------------------------------------------------------

Exists in `Z/` today — bind to it, do not re-port:

| Z/ item | What it is | This spec's use |
|---|---|---|
| `src/meta/dlm.zig:1–399` the 4a `Manager` (`lockMany` canonical `I`-then-`D`, `tryLockMany` the ONE sanctioned try-only addition, `Guard`/`GuardSet` movable by value, census + `lock_phase_ns.dlm_guard_wait/hold`) | level 4a | the local half of every guard set; the holder's `Take` arm; `tryLockMany` is the late-guard arm (§2.9) |
| `src/meta/tx.zig:105–133` `KvTx.cross: CrossVolume = refuse\|independent\|child_first\|parent_first` and `token_quiet: bool` ("the tx mutates no record a read token carries … so the conveyor pass recalls nothing for it") | the rung-5 cross-member tx shapes; the recall exemption word already on the tx | `cross` is RETIRED by §2 (one intent replaces the ordered two-entry split); `token_quiet` is read by §1.4 |
| `src/meta/meta_set.zig:11–23, 389–505` `MetaSet.commit` → `commitSplit`: two ordered entries by `t.cross`, `EXDEV`-class refusal otherwise (`routing_cross_volume_refused`), `routing_split_commits/_partial` (`routing/stats.zig:58–67`) | the stopgap this spec replaces | deleted with §2 (the stats rows retire; the rail goes red on an unread row — delete them) |
| `src/meta/conveyor.zig:27–44, 1069–1310` stage A steps A0–A13 | the pass | §1.4 inserts the recall between A1 and A2 |
| `src/fuse/handler/kv_fs.zig:1257, 1421, 1489, 1563–1567, 1634` the five `t.cross = …` sites (destroy batch, create, unlink, rename "two parents on different members … answer EXDEV (the symmetric plane's cross-owner intents are rung 6's)", link) | the stopgap's callers | each becomes a plan builder (§2.5) |
| `src/meta/economy.zig:331, 362, 440, 499, 520, 599` `stageCreate/Mkdir/Link/Unlink/Rmdir/Rename`, `GuardPlan` :134, `SharedTimes` :191 | the one-member stagers | the ONE applier (§2.6) stages through the same functions, one step per tx |
| `src/fuse/transport/session.zig:810–826` `notifyInvalInode(ino, off, len)`, `notifyInvalEntry`, `notifyPrune(nodeids)` on the sideband; `transport_notify_{sent,enoent,failed,dropped}` :128–132 | the notify writes (charter §4) | §3.4's holder-side kernel invalidation |
| `src/data/layout.zig:1303 PublishLocks, :1392 RefsSeam, :1430–1447 PublishArm{diff, flip_to_kvmap}, :1466 publishLayout`; `src/data/plane.zig:785 refsSeam, :815 lockLayout` (level 3.7) | the local layout publish | §3.5: the SHIPPED publish is the same `publishLayout` run AT THE HOLDER |
| `src/fuse/handler/op_guard.zig` (level 1), `lifecycle.zig:193 dispatch` | the op guard / dispatch | §1.9: the divert sits BELOW the op guard, never inside a backend I/O under it (0002 row 84) |
| `src/core/knobs.zig:119` `SQUEEZEFS_DLM_STRIPES` (`ab_lever`) | the one 4a knob | unchanged |
| `src/model/explorer.zig` (`expectHolds`/`expectFails`), `src/sync/{notify,rwlock,mutex,semaphore,flight}.zig` | the model + primitive stock | §1.5, §1.7 models; every park below is a `sync.Notify` register-then-check |

Missing (this spec): the token plane (holder + reader), the pass hook, the reader sink, the `-o ro`
token/quiescent arms, the read divert, the intent record + the ONE applier + the shipper + the
cadence, the travelling guards, the `dir_rename` lease + exact ancestry, the record-level ship +
the served-mutation sink, striping whole, the three gauge families, fsck C17.

Rung-6 prerequisites this spec ASSUMES (the sibling spec "appenders/leases/manager"): the slot
lease table (`is_leased(slot)` / `resolve(slot) → Holder{appender_id, g} | Unleased`), tree 0's
`slot_state:{s}`, the appender→endpoint table (`SlotHolderCache`, `R/src/slot_holder_cache.rs:1–28`),
the membership owner (`lease_deadline_ms(client)`, `T_owner` = 45 s, `R/src/fuse_client.rs:195`),
the commit door (`SlotBusy{slot, holder, g}`), the cluster wire (one listener per mount, a typed
`RefusalClass` word on every refusal — `R/src/error.rs:12–71`: `SlotMoved{slot,holder}`=1,
`StaleHolderView`=2, `HolderUnreachable{holder}`=3, `MembershipPending`=4, `ListenerRefused`=5,
`StaleOwnerEra`=6, `HolderDeferred`=7, every one EAGAIN never EIO/EINVAL — charter §5.5).

---------------------------------------------------------------------------
## 1. The token model — GPFS-strict, the ONLY foreign-read method
---------------------------------------------------------------------------

### 1.1 What a token is; what a grant carries

- **Granularity = the OBJECT** (design :644–646): an inode with its CARRIED xattrs, or a directory's
  dentry set (per STRIPE once striped — a stripe ino is just another object, :644). Never the slot.
  Modes: `Read` (shared, N holders) and the holder's implicit `Write` — "the lessee needs no token on
  its own tree" (`token_plane.rs:106–112`: `enum TokenMode { Read }` — the only wire mode).
- **The carried xattrs** = `token_carried_xattr(name)` = the user-visible allowlist ∪ `layout` ∪
  `system.symlink` (`token_plane.rs:369–382`). Every other internal record (`writer_claim`,
  `client:`, `job:`, the planes' own) is the CONTROL plane's — projected, never tokened, and "its
  write recalls nothing" (:375–377) — which is what `KvTx.token_quiet` says (`backend.rs:27848,
  27875` set it iff `!token_carried_xattr(name)`; `Z/src/meta/tx.zig:131–133` already has the word).
- **One grant PAGE carries** (`TokenRecords`, :271–279): `attrs` (every page — one fixed word),
  `xattrs: [(name, value)]` + `xattrs_complete`, and for a directory `dir: ?(entries, complete)`.
  `TokenWants{dentries, records}` (:122–145): the first page asks `records` (+`dentries` for a
  directory); a continuation page sets `records = false` so "no page re-carries what an earlier one
  did" (:114–119, review Issue 16). The holder builds a page in `token_records_for`
  (`backend.rs:4348–4432`): the FOLDED inode (pending times folded in, :4359), the carried xattr
  names SORTED, from `xattr_after`, under ONE byte budget `grant_dentry_budget()` =
  `CONTROL_MAX_FRAME_BYTES / 2` = 512 KiB (`token_plane.rs:394–396`; "one value (≤ the record cap)
  always fits an empty page", `backend.rs:4377–4381`); the dentry page runs only once the xattr set
  is complete (:4387–4390) and resumes STRICTLY after cookie `after` (0 = start), each `DirRecord
  {cookie, child_ino, file_type, name}` (:257–262) charged `24 + name.len()` (:4405).
- **The reader's cache entry** `TokenEntry{attrs, xattrs, dir: ?[]DirRecord (cookie order), names:
  name → index, bytes, state LIVE|REVOKED, used}` (`token_plane.rs:1948–1961`): a `lookup` is one hash
  probe (:1964–1969), a `readdir` page is one `partition_point` on the cookie-ordered set
  (:1974–1980). Immutable once installed — a serve that began before the recall serves the records it
  holds, "linearizable at its start"; the DMA hazard is the `ServeStamp` drain's, not the entry's
  (:1942–1947, Issue 15).

### 1.2 The wire (verb block, frames) — v4 shape

Reference: its OWN verb block `0x0600..=0x06FF` with ONE verb `VERB_TOKEN_CALL = 0x0600` carrying a
`TokenRequestFrame{schema, request_id, volume: u16, client: String, call: TokenCall}`
(`token_plane.rs:80–84, 353–359`), `TokenCall ∈ {Grant{object, mode, wants, after, xattr_after},
Recall{wait_ms}, RecallAck{frame_id}, Release{objects}, CustodyGrant{object, span, concurrent_write,
wait_ms, lease_epoch}}` (:150–190), `TokenReply ∈ {Granted{records, already}, NotHolder{holder}, Gone,
Recall{frame_id, objects}, Acked, Released{count}, Refused{reason}, CustodyGranted{grant, records,
already}, CustodyRefused{status, reason}, Rejected{reason}, HolderDead{holder}, NotMember}` (:282–347);
statuses `OK / SCHEMA / MALFORMED / NOT_HOLDER / REFUSED = 56 / REJECTED = 57` (:86–104); bodies
bincode "encoded unbounded, decoded bounded" at the CONTROL cap 1 MiB (:384–396,
`cluster_wire.rs:215`). Objects are LOCAL key inos of the frame's volume (:46–48).

v4 (decision 0002 rows 2/53: no serialization-library wire): every token frame is an `extern struct`
head + length-framed bodies, little-endian, xxh3-64 over the frame, decoded total under the CONTROL
cap, every reserved byte checked zero.

```
TokenHeadWire (32 B): magic "SQZTOKEN", version u16 = 1, verb u8, status u8,
                      volume u16, reserved u16 (= 0), request_id u64, client_id u64, body_len u32, pad u32
  verb:   1 Grant, 2 Recall, 3 RecallAck, 4 Release, 5 CustodyGrant
  status: 0 Ok, 1 Granted, 2 NotHolder, 3 Gone, 4 RecallFrame, 5 Acked, 6 Released,
          7 Refused, 8 Rejected, 9 HolderDead, 10 NotMember, 11 CustodyGranted, 12 CustodyRefused
GrantAskWire (32 B):  object u64, after u64, wants u8 (bit0 dentries, bit1 records), mode u8 (= 0 Read),
                      reserved[6], xattr_after_len u32, pad u32 ‖ xattr_after bytes
GrantPageWire (96 B): attrs = InodeValueWire (68 B, Z/ v4) ‖ flags u8 (bit0 xattrs_complete, bit1 has_dir,
                      bit2 dir_complete, bit3 already), reserved[3], n_xattrs u32, n_dir u32, pad[16]
                      ‖ n_xattrs × {name_len u16, value_len u32 ‖ bytes} ‖ n_dir × {cookie u64, child_ino u64,
                      file_type u8, name_len u8, reserved u16, pad u32 ‖ name}
RecallFrameWire:      frame_id u64, n u32, pad u32 ‖ n × object u64   (frame_id 0 ∧ n 0 = "park elapsed")
RecallAckWire:        frame_id u64;   ReleaseWire: n u32 ‖ n × object u64;   NotHolderWire: holder u32
```

`client_id` is the member's 64-bit identity word (the KD-MW-2 member id, hashed — never a `String`
on a control frame; the `TOKEN_CLIENTS` set below keys on it). The `CustodyGrant` body is the data
plane's custody ask (`span`, `concurrent_write`, `wait_ms`, `lease_epoch`, :183–189) — defined by the
custody spec; this one only requires that its reply CARRIES a `GrantPageWire` (§1.3 step G5).

The frame law every verb obeys (`token_plane.rs:1657–1813` `serve_frame`): schema check → the volume
ordinal (`TokenSetService`, :1856–1880) → the plane armed (`STATUS_NOT_HOLDER` otherwise, :1669–1675)
→ the appender PARK gate ("a parked lessee is still the lock master … a park that EXPIRED poisoned
custody", :1676–1689, `dlm_token_park_expired_refusals`) → **the membership lease FIRST** (:1690–1721:
`lease_verdict(client) == Expired` ⇒ `NotMember`, `dlm_token_nonmember_refusals`, nothing registered)
→ `note_token_client` (:1722–1725 — every member that reached a token verb is a TOKEN client; the
recall-gated free reads the set against the census, :633–656) → dispatch.

### 1.3 The protocol, step by step

**G — a first touch** (reader `fetch_once`, `token_plane.rs:2722–2823`; holder `serve_grant`,
:975–1093):

1. The reader resolves the object's HOLDER off its own tree 0 (`reader_holder_of`,
   `backend.rs:3716–3733`: `slot_state:{slot}` `Leased{appender_id ≠ 0}` ⇒ that appender; unleased or
   the manager's own ⇒ the manager's plane) and picks the per-holder plane (`token_reader_for`,
   :3746–3769; `reader_plane_for_holder` :3782–3830 binds an unbound holder ONCE per epoch step off
   durable state, else refuses EAGAIN `dlm_token_reader_unbound_holders` — "the bounded-staleness
   projection is never served in its place", :3818–3830).
2. The reader reads `gen0 = revoke_gen(object)` (:2723), then issues `Grant{object, Read, wants{
   dentries, records: !records_done}, after, xattr_after}` pages in a loop (:2731–2810): xattr pages
   first (`xattr_after` = the last name carried, :2760), an EMPTY incomplete page is refused never
   spun on (:2755–2759), then dentry pages by cookie until `complete` (:2767–2775).
3. The holder: the slot's lessee first (:985–995) — a slot another appender leases ⇒ `NotHolder{holder}`
   (`dlm_token_not_holder_redirects`), a DEAD lessee ⇒ `HolderDead{holder}` (`slot_resolve_dead_redirects`,
   never a dial). Then **register-then-check** (§1.5): `gate.grant_register(object, client, lane)`
   (:1016) — the lane records `(object, client)` BEFORE any read; `already` = it was held. A pending
   recall of this `(client, object)` is WAITED OUT first (:1021–1037, `dlm_token_regrant_under_recall_
   waits` — "served on the doomed registration, the token stood at the reader untracked here"); an
   object IN FLIGHT parks until the settle (:1042–1050, `await_object_settled` bounded by the recall
   deadline — past it `Refused`, never pre-commit records); the records are read (:1051–1053); a pass
   that took the object in flight DURING the read re-parks and re-reads (:1054–1070); a recall that
   landed inside the read repeats (:1071–1076); `Gone` retracts a fresh registration (:1077–1081).
4. The reader installs under `gen0` (`install_records`, :2923–3012): bytes charged = 128 + Σ xattr bytes
   + Σ (24 + name) per dentry (:2004–2007, :2931–2940); ONE entry over the whole records budget is
   refused loud (`dlm_token_oversize_refusals`, :2941–2953); room is made by VOLUNTARY RELEASES
   (`evict_to_budget` :3030–3060 — second-chance `used` bit, then any; each retired entry drained +
   purged THEN `Release`d, :3065–3075); the install is conditional on `revoke_gen(object) == gen0`
   DECIDED UNDER THE CACHE ENTRY (:2975–3007 — "the recall handler bumps the generation BEFORE it
   removes the entry, so a bump the check sees aborts the install, and a bump after the check finds
   the installed entry"); a dentry-bearing entry is never displaced by a bare one (:2986–2992).
   A fetch that spanned a recall retries (`RecalledMidFetch`, `dlm_token_fetch_retries`, :2713–2716).
5. Single-flight per object (`fetching` map, :2690–2709): the loser waits the winner and re-reads the
   cache. Hits: `entry.state == LIVE ∧ (!wants.dentries ∨ dir != null)` (:3172–3179).

**R — the recall** (holder `recall_and_wait`, :1328–1460; reader `run_recall_channel` :3229–3357 +
`handle_recall_on` :3360–3399):

1. The holder's pass takes its UNION in flight (`gate.pass_begin`, :1336 — mark THEN read holders,
   §1.5), sweeps DEAD readers' grants first (`sweep_expired` :899–924: a client the owner sees
   `Expired` loses every grant NOW; the pass waits for nobody on its account), then
   `lane.recall_object(o, now)` per object with holders (:1351), `dlm_token_recall_fanout.record(holders)`,
   `recalls += n`; a pass that recalled anything is one `dlm_token_recall_batches` (:1359).
2. `lane.issue_pass` builds ONE in-flight frame per client (`RecallFrame{client, frame_id, inos}`,
   `tokens.rs:986–`; ≤ `batch_max` = `1 MiB / 2 / 32 B` = 16 384 objects/frame, `tokens.rs:829–834`)
   into `pending_frames[client]`; `frame_wake` wakes the readers' standing polls (:1372–1388).
3. The reader's channel task holds a long-poll `Recall{wait_ms}` on its OWN session (:3271–3272 — "a
   parked poll must never block a grant"); the holder answers the next frame or `{0, []}` at the park
   bound `poll_park_bound(deadline) = clamp(deadline/4, 100 ms, 1 s)` (:475–480, :1097–1127).
4. On a frame the reader, PER OBJECT: `revoke_gens[o] += 1` FIRST, then removes the entry (REVOKED,
   bytes credited) (:3369–3381); then **the data-plane half BEFORE the ack** (§1.7) — `sink.
   drain_and_purge(recalled)` (:3383–3388); then `RecallAck{frame_id}` on the SAME session (:3389;
   "the next call on the channel carries the acks", :3296–3298). `recalls_acked += n` only on the
   holder's `Acked` (:3390–3393).
5. The holder's `serve_ack` (:1147–1163): `lane.ack_frame(client, frame_id, now)` retires the frame's
   grants; a stale ack matches nothing and is logged DEBUG (:1153–1158); `ack_wake` wakes the pass.
6. The pass waits until every object of its union has `holders_under_recall == 0` (:1398–1403 — "the
   pass waits for the recalls it ISSUED": a grant registered after the union went in flight is
   parked on the gate, never counted, else "every pass held to that grant's park bound — the storm
   legs' 18.75 s stall"). The wait = min(next ack, the earliest LIVE lease's remaining, the tick
   `deadline/4` floored 1 ms) (:1370, :1404–1413). Each wake: `sweep_expired` over the recall clients
   (:1416), then `lane.expire_overdue(now)` — a frame past the DEADLINE on a client judged `Expired |
   Unknown` is `expired_with_lease`; on a `Live` client it is **`dlm_token_recall_timeouts_live`**
   (must-stay-0) + the tripwire `dlm_token_recall_timeout_live` + `free_grace::note_recall_unacked_
   live()`; the token is retired and the commit PROCEEDS (:1417–1440).
7. `dlm_token_recall_rtt_ns` EXACT-SUM per batch (:1442–1458): `send` = call → the last frame handed
   to a poll (`last_send_ns`, :1106–1107), `drain` = → the last ack's arrival (`last_ack_ns`,
   :1149–1152), `ack` = → the pass observing it; cuts clamped monotone into `[t0, t_end]` so
   `send + drain + ack ≡ total` to the nanosecond.
8. After the apply (or the failed window's rollback): `plane.settle(union)` (:1510–1514) — the gate's
   refcount drops, parked grants wake (`inflight_done`).

**A — the ack conditions** are §1.7. **L — release** (`serve_release` :1165–1174; reader
`release_retired` :3065–3075): voluntary, drained + purged first, best effort ("a lost release costs
the holder one needless recall, never a wrong answer", :3071–3072). **The clean leave** `stop()`
(:3424–3447) releases every held token when the channel is ok OR alive; death (`test_die`/kill)
releases nothing — the holder's lease arm retires the grants.

**G5 — `CustodyGrant`** (`serve_custody_grant`, :1188–1314): the S9 write-custody acquire served by the
SLOT HOLDER in ONE round trip that also carries the file's read token — screen the wire words first
(`screen_custody_words` :528–569: slot ≤ `FOREST_SLOT_MAX`, raw local ≥ 2, a slot this forest minted,
`span [start, end)` non-empty; `screen_custody_object` :583–615: a durable record that is a REGULAR
FILE — "a directory is never a custody object: not a striped directory, not a stripe ino, not a
marker target"; a failure is `Rejected` + `dlm_token_custody_rejected`, must-stay-0), the lessee
check, the handover/quarantine deferrals (`CUSTODY_DEFERRED`, :1237–1262), the arbitration, THEN
`serve_grant` under the same gate (:1301–1313; a token that cannot be served RELEASES the custody —
"the caller adopts both words or neither"). The reader installs the carried page under its `gen0`
(`install_carried` :2844–2916; an incomplete xattr set is PAGED to completion first — Issue 4).

### 1.4 The holder's recall-before-admission — where in A0–A13

Reference: ONE call `recall_tokens_for_batch(&batch)` at the top of `run_batch_group`
(`backend.rs:4442–4455`, the site :22979–22995): the union over every record of every tx with
`!q.token_quiet` through `record_object_ino(kind, key, forest)` (:19484–19494: the ino at offset 0 of
every ino-major key — inodes, dentries BY PARENT, xattrs, block map; refs by their OWNER ino at the
tag offset), `union_objects` sorted + deduped (`token_plane.rs:4100–4105`); "under the batch's 4a
guards, BEFORE the ring admission and any node lock — and before the region-growth Dekker, which
has nothing to do with a recall and must not be held for its round trip" (:22979–22986); its wall is
`pass_token_recall` so `pass_total` decomposes (:22986–22992); `plane.settle(&recalled)` after
`run_batch_pipeline` (:23049–23055); the failed window's ROLLBACK is the gate's second user around
its removal (`token_plane.rs:708–718`, Issue 14/23).

Z/ (`conveyor.zig:27–44`): the recall is **A1½** — after A1 (the liveness flags) and BEFORE A2 (ring
admission): `A1 → A1½ recall_and_wait(union of the batch's token objects) → A2 admission → … → A9
apply → A12 submit → A13 handoff → settle(union)`. The union is computed from the drained batch's
`QueuedTx.frames()` keys (`tx.zig`), skipping `token_quiet` txs; a batch with no armed holder plane
pays ONE null probe (the reference's `OnceLock`, :22988). The settle runs in `Window.finish`'s
terminal outcome for both arms (fan-out AND rollback), so an object leaves the flight with its LAST
user. Measured (`.benchmarks/…pr5-tokens.md` §3): the pass's recall of a 64-reader broadcast =
2.30 ms (`send` 1.38 + `drain` 0.40 + `ack` 0.51 ms, in-process loopback, debug).

### 1.5 The register-then-check park — the model and its weakenings

`GrantPassGate` (`token_grant_core.rs:84–160`): `inflight: Mutex<BTreeMap<object, users>>` — a
REFCOUNT, not a set (:36–47). `pass_begin(objects, table)` (:100–108): increment every object's
count under ONE lock, THEN read each object's holders through the lane's lock. `grant_register
(object, client, table)` (:131–144): `table.register` (the lane's lock) THEN `is_inflight` (this
lock) ⇒ `Park | Proceed`. `settle(objects)` (:114–127): decrement; an object leaves at 0.

The law (:9–34): both tables are mutexed and each side takes BOTH locks one per step, so the locks'
acquire/release chains order the steps — "no extra fence is needed and none is claimed". One side
always sees the other: a grant concurrent with a pass is either RECALLED (the pass saw the holder)
or PARKED and served the post-commit records (the grant saw the mark) — never neither.

Loom models (`loom-models/src/lib.rs:7513–7654`) and their weakenings, to be re-expressed as
`Z/src/meta/token_gate_model.zig` under `model/explorer.zig`:

| Model | Holds | Weakening (`expectFails`) |
|---|---|---|
| `a_first_touch_grant_racing_the_pass_is_recalled_or_parked` (:7561–7583) | `holders ≥ 1 ∨ admission == Park`; after `settle` nothing in flight | the grant's two steps SWAPPED (in-flight read before the registration — the first build's order): loom finds `holders == 0 ∧ Proceed` (:7524–7528) |
| `two_readers_racing_the_pass_are_each_recalled_or_parked` (:7589–7613) | `holders + parked ≥ 2`, `holders ≤ 2` | the same swap reads `holders 1, parked 0` (:7528) |
| `two_overlapping_users_keep_the_object_in_flight_until_the_last_settles` (:7625–7653) | a grant registered INSIDE user 2's window (after its holders read) PARKS whatever user 1 does | the refcount replaced by a SET: user 1's settle clears user 2's mark and the grant reads `Proceed` against a window in progress that recalled nobody (:7620–7624) |

"Removing `SeqCst` fences between the steps changes nothing — the mutexes already order them —
which is why none is in the code" (:7529–7530). In Z/ the two tables are two `sync.Mutex`-guarded
maps (control plane: one lock per pass on an ARMED holder, never the unarmed hot path, `token_grant_
core.rs:44–47`); the model is a plain value type with explicit per-thread program counters
(`AGENTS.md` "Model checks"), store buffering not modelled (the mutex is the fence).

The reader-side counterpart (install-under-generation, `token_plane.rs:2975–2981`): bump BEFORE
remove on the recall; check `gen == gen0` UNDER the cache entry on the install. Model it too
(`token_install_model.zig`): weakening = remove-then-bump, which lets a fetch that read pre-commit
pages install after the recall removed the old entry.

### 1.6 The reader-side cache, the serve gate, the channel, `SlotHolderCache`

- **Planes**: one `TokenReaderPlane` per `(volume, holder)` (`backend.rs:3734–3769`: the manager's
  plane is the default for own/unleased slots; a per-holder plane is dialed lazily on the bound
  endpoint "with the same identity, secret and volume ordinal"). The grant session POOL (:2074–2080,
  :2536–2541: depth = the D-1b derivation, "one per 8 cores, 2..=8", dialed lazily; a grant PARKED at
  the holder holds one session, the rest ride the others; `dlm_token_grant_sessions{,_dialed}`).
- **The serve gate** (`serve_gate` :2523–2534): refuse EVERY serve when the reader's own membership
  lease is past `T_self` (`lease_live` :2440–2447; `drop_all` — "a reader that cannot be recalled
  serves NOTHING") or the recall channel is not FRESH (`channel_fresh` :2453–2462: a completed round
  within `2 × park_ms + DELEG_FRESH_SLACK_MS` — park 1 000 ms, slack 2 000 ms, `tokens.rs:1907,
  1911`); the stale channel is the WIRE class `Io(NotConnected)` → EIO (:2229–2235 — the root-attr
  projection arm reads it as transient), the law words are `Refused{EIO}` (:2213–2221). The FIRST
  resolve after an arm parks on the channel's first round instead of failing on the dial's latency
  (`await_channel` :2486–2514 — register-recheck on `channel_round_wake`, bounded by the window;
  `ChannelWait::Failed` returns early for a dead/moved holder, :2475–2484). A `NotMember` refusal
  parks the serve on the member's reclaim (`serve_gate_parking` :3102–3143, `dlm_token_membership_
  waits`, bounded by `reassertion_wait_bound`; past it `MembershipPending`, never EIO).
- **The channel task** (:3229–3357): dial → `Recall{wait_ms}` → on `Recall{frame_id, objects}` the
  round is FRESH (`channel_ok`, `channel_last_round_ms`, `channel_failed = false`, `dlm_token_channel_
  rounds`) and a non-empty frame is handled + acked on the SAME session; any error ⇒ `note_channel_
  failure` + `drop_all_and_purge` ("every token is dropped (fail-closed) until a round completes",
  :3342–3353) + backoff `[DELEG_PARK/20 = 50 ms, ×2, ≤ 5 × DELEG_PARK = 5 s]` (:2013–2016) cut short by
  a re-point (§1.10); the backoff resets at a completed ROUND, never at the dial (:3250–3256).
- **Budget**: `records_budget_bytes = max(mem_budget / 256, CONTROL_MAX_FRAME_BYTES)` (`tokens.rs:
  107–128` — 64 MiB at 16 GiB ≈ 1.6 M dentries or ≈ 1 000 full `layout` values; the floor is the
  physical minimum under which one grant page could be resident); registered with the R5 authority as
  component `dlm_token_records_bytes` floor 0 weight 1 (`token_plane.rs:2029–2056`); the authority's
  shed DROPS entries WITHOUT a release (`shed_all` :3083–3091, `dlm_token_sheds` — "the holder keeps
  the grant and recalls a reader that holds nothing, one needless recall per shed object, never a
  wrong answer").
- **`SlotHolderCache` link** (`R/src/slot_holder_cache.rs:1–28`; design :322–324): `slot → (appender_id,
  g, endpoint)`, `ArcSwap`'d, fed from tree 0 at the arm and at every epoch poll, one entry learnt per
  grant / forgotten per release the manager writes, `ResolveSlot` the stale-view fallback counted
  `slot_resolve_rpcs` (NOT `dlm_rpcs`), `NotHolder{current}` redirects followed ONCE
  (`slot_resolve_redirects`; a READER follows too since PR 13 defect 28, `backend.rs:4131–4150`,
  `dlm_token_reader_redirects_followed`), a lessee the owner lists DEAD is `HolderDead` never dialled.
  Invalidation: the grant's `dead_members`, and a `g` change observed in tree 0.
- **`token_cache_core.rs`** (:48–151) is the S8 FENCING-token cache's word protocol (`TokenSlot.merge`
  = `fetch_max(token) + fetch_max(term)` monotone — weakening: load-compare-store regresses a racing
  higher grant; `EraFloor.record` monotone; `record_grant_ordered` = floor BEFORE publish — weakening:
  flipped, a retired entry exposes a miss floored below its own era; `loom-models:5683`). It is NOT
  the read-token cache; its only read-token relevance is the second-chance `used` bit the
  `TokenEntry` reuses (:1960, :3041). Port the monotone-word lesson where a generation is merged
  from reordered replies (the reader's `revoke_gens` are per-recall increments, never merged).

### 1.7 The reader's ack conditions: DMA drain, census purge, DIRTY kept

`MountRecallSink::drain_and_purge` (`token_plane.rs:3928–3960`), in order:

1. **`ro_coherence::drain_in_flight_serves()`** (`ro_coherence.rs:508–523`): a STEP on the reader's
   layout cache (`note_reader_epoch_step` — every layout entry stamped before it misses,
   `layout_entry_pre_step` :122), then the OBSERVED drain `serve_drained_below(gen)` (:395–418) parked
   on `DRAIN_WAKE` (the completion's `notify_one`, :372–375) with a 10 ms ticked backstop (:520).
   **Per-thread serve stamps**: `ServeStamp::begin()` at a read serve's START (before it resolves any
   binding, :310–345) — `fetch_add(1, SeqCst)` on `shard[thread].slot[gen % 64]` then a `SeqCst`
   re-read of the generation (the Dekker re-read: a step between the load and the increment is
   observed, the serve counted one slot early — conservative, `reader_serve_step_races`); `Drop`
   (:357–376) `fetch_sub` + "wake the ladder when this completion may have drained a slot it waits
   on" (`DRAIN_WAIT_GEN` stored `SeqCst` BEFORE the slot loads ⇒ no lost wake by construction).
   Shards = `next_pow2(process_parallelism) ∈ [1, 64]`, one per thread round-robin (:263–287,
   `#[repr(align(64))]` — "shards never share a line", :230–233); 64 generation slots = `u64::BITS`
   (the poison mask's width, :224–228); a slot re-stamped while occupied is POISONED and counted
   `reader_serve_slot_overruns` (must-stay-0, :212–216). Per-op cost unarmed: one relaxed load
   (:218–222). Z/: `counters.ShardedCounter`-class shards with the two `SeqCst` points kept and
   a model (`serve_stamp_model.zig`) whose weakening (the re-read relaxed) lets a serve escape the
   drain. **Port: DELETED (decision 0002 row 560)** — no production serve ever stamped (the drain
   was vacuous), and the hazards it guarded are closed elsewhere: a block deposited or served under
   a revoked binding meets the read path's binding recheck AFTER its bytes are in hand, and the
   kernel orders its caches behind the invalidations (`INVAL_ENTRY` takes the parent's `i_rwsem`
   an in-flight lookup/readdir holds; `INVAL_INODE` bumps `attr_version`, dropping an in-flight
   GETATTR reply, and waits out in-flight reads' locked folios). The push's reply IS the ack;
   `RecallAck` and the deferred verdict are gone.
2. **The scoped purge** (`purge_scoped` :3883–3925): for each recalled object with an entry, the block
   keys its `layout` names (`layout_block_keys` :3798–3822 — v4: the bindings off `data/layout.zig`'s
   `MappingWire`, never strings — 0002 row 105), `router.discard_clean_layout_entry(global)` → one of
   `Discarded | DirtyKept | HeldSkipped`, `note_recalled_object(global)` (the FUSE-layer hook, §3.4),
   `cache.purge_block_key(k)` per key through the ONE purge (`data/purge.zig` in Z/, 0002 row 138);
   `dlm_token_recall_purged_keys/_scoped_purges`. An object whose keys cannot be enumerated (no entry
   held — shed or never granted — or an off-record kvmap head) falls back to the WHOLE census ONCE
   per call (`purge_reader_block_keys`, `dlm_token_recall_census_purges`, :3936–3951).
   Z/ (0003 §W.2, 0002 row 513): the revoked `Entry` stays pinned through the purge and the sink
   enumerates its carried `layout` (`DataPlane.recallPurgeLayout`); the census drops FOREIGN keys only
   (a cell this member never touched) through `purge.purgeCensus`; instead of stamping every block
   fill, a binding loss after a fetch that may have deposited a foreign key purges it, and the
   deposit-only paths (dehydration, the hold-serve ceremony) never deposit one.
3. **A DIRTY layout entry is KEPT** (:3857–3882, PR 13b — the `sym-foreign-file` leg's acked-write
   loss): under the divert a WRITER is a token client of the holders it writes to, and its
   `layout_dirty` entry IS the pending write ("an inline write's `data_key` is the ONLY copy of the
   acked bytes until the fsync's save"); the recalling commit is OLDER than that write, so the entry
   is newer than anything the recall could re-fetch — dropping it "made the fsync find nothing to
   save and return 0 with the bytes gone". Verdict `DirtyKept` → `dlm_token_recall_dirty_kept` (0 on
   a `-o ro` reader); the decision runs under the ino's (3.5) stripe in its NON-PARKING form, a held
   stripe SKIPS the discard (`HeldSkipped`, `dlm_token_recall_discard_skipped`) — safe because the
   step already made every clean entry pre-step. Z/: the write side's `write_epoch.Epochs.closing`
   visibility law (0002 row 178) is the dirty authority; the discard asks it.

Then the ack travels. Closure (design :1009): `dlm_token_recalls ≡ dlm_token_recall_acks +
expired_with_lease` per holder.

### 1.8 `-o ro` mounts: reader TTL 0, the quiescent reader, `ESTALE`

- A `-o ro` mount is a MEMBER (it needs a lease so its tokens can expire) and a token client — "no
  claim, no PR key, no appender region, nothing written" (design :641); it pays exactly a writer's
  foreign cost: 1 RPC per object first touch, 0 while cached, a recall when the object changes.
- **Every reader TTL derives to 0 under tokens** (`.benchmarks/…pr5-tokens.md` §1.2): the attr/entry/
  dir/negative kernel TTLs and the daemon cache TTL all derive from `metadata_staleness_bound()` =
  0 — "a cache may hold an entry exactly as long as freshness is proven, and under a token that is
  'until the recall', which no TTL expresses"; `reader_staleness_bound_ms` reads 0 (kept for tooling,
  design :643). An explicit kernel TTL on a token reader REFUSES loud (`readonly_mount_tests.rs:1739`).
  Z/: `fuse/mount_options.zig`'s per-class TTLs take the derived 0 on a token reader; a `-o *_timeout`
  on one refuses naming the token law.
- **The quiescent reader** (design :645; `revalidate.rs:480–524`): a `-o ro` open of a set with NO
  live writer (no exclusive writer lock, no heartbeat-fresh `writer_claim` on any volume —
  `live_manager_present`) arms no token client (`ReaderArm::Quiescent`, `reader_quiescent` 1), serves
  the checkpointed projection its open replayed (exact — nothing writes), and its poll watches for a
  writer by BOTH faces: a same-host writer's exclusive lock, and a remote writer's FIRST CHECKPOINT —
  `revalidate_reader` on a quiescent volume with `epoch.ledger_seq > current` answers `writer_appeared`
  and REFUSES the swap (:489–505; "adopting it would serve a live writer's state from a projection,
  R-SYM-4's second method"). Either face FAIL-STOPS the reader once: every metadata op answers the
  typed `ReaderFailStopped` → **`ESTALE`**, the root's attributes excepted so `.stats` reads;
  `reader_quiescent_writer_appeared` 1; one loud line naming the remount. Owed in the reference: the
  in-place join (:645 last sentence) — Z/ may land it as the arm at once (the reader has a lease to
  take and a holder to dial; the fail-stop is the cheaper stopgap).
- The token arm on the mount path refuses loud on a flat/`--single-writer` volume, without a
  membership lease, without the endpoint or the secret (`.benchmarks` §1.2; `readonly_mount_tests.rs:
  1335, 1369`); `SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP=0` is refused on a token reader (:1698) — in Z/
  the lever does not exist (§7).

### 1.9 The read divert — every writer is a token client of the holders it writes to

`token_serve(object, wants)` (`backend.rs:3983–4003`) is the ONE serve the five read verbs (`find_
dentry`, `getattr`, `readdir_page`, `getxattr`, `listxattr`) and the plan builders' witness
(`read_inode_witness`, §2.2) take: `Ok(None)` = read locally; `Ok(Some(None))` = gone at its holder.
The WRITER's sync "read locally" verdict (`writer_reads_locally` :4013–4022) decides on the gate's
bits alone — unarmed, a slot this mount leases, or an unleased slot on the manager (which maintains
every unleased tree itself) — "no table lock, no allocation"; the flat mount's cost is two `Option`
probes (:3989–3998, PR 13c gate 1: the box's `perf` read the added route-table loads on the handler
lanes of the flat mdstorm). The armed body (`token_serve_armed` :4025–4150): resolve the plane; on
`NotHolder` a WRITER re-resolves ONCE inside the same op (:4116–4150 — "the manager's word is fresher
than the projection the ledger cadence hands a joiner"); a JOINER whose holder is unreachable
re-projects tree 0 once (keyed on the ledger seq) and, if it moved, resolves again; a holder that
did not answer gets ONE ask of the MANAGER's word (`manager_word` :4073–4092, "F2: never a second
dial at a dead address" — the storm leg's reader parked 24 min behind a dead holder). Z/: the
divert sits inside `KvFs`'s read verbs BELOW the level-1 op guard and BEFORE any (3) guard (0002
row 84's drop-before-I/O law); `meta_set.zig` grows `tokenServe(object, wants) !?TokenServe`.

### 1.10 Readers FOLLOW a moved listener

`TokenReaderPlane::repoint` (design :641 "As built at PR 13"; `token_plane.rs:2063–2073`): a
resolve whose plane's channel FAILED or never freshened re-resolves the holder's endpoint off
DURABLE state (its page identity → its claim-set entry — a control xattr read off the reader's own
S5 projection, no wire), and a MOVED endpoint re-points the plane IN PLACE: the `endpoint` word +
`endpoint_gen` swapped so every session dialed under the old generation drops before its next call
(:2069–2071, :3241–3246, :3279–3283), every cached token dropped with its purge ("a holder that moved
may have re-granted"), the channel task woken out of its backoff (`repoint_wake`). A per-holder plane
(a joiner rejoined at a new port) is stopped dead and the successor's dialed. The same address keeps
the fail-closed posture. `dlm_token_holder_repoints`; `backend.rs:3962–3969` logs the move.

### 1.11 The S5 epoch-poll survives as the CONTROL plane only

`read_newest_ledger_from(path, base, adopted_seq)` (`revalidate.rs:358–410`): checkpoint `seq` sits
in slot `seq % 32`, so the next record "can only sit in ONE slot" — read that 4 KiB slot, walk
successors while each holds the next seq (a writer that raced ahead by more than the ring lands a
higher seq of the same residue — adopted the same way), stop at an OLDER record or an all-zero
slot; a slot that decodes as neither (TORN) takes the whole-ledger fallback (`meta_kv_revalidate_
ledger_full_reads`). Measured (`.benchmarks` §3): idle poll 4 KiB (was 128 KiB — 32× fewer bytes),
2 new checkpoints = 12 KiB, a torn slot 4 KiB + 128 KiB. **The gap belt** (`read_root_epoch`
:453–472, PR 13 defect 25): a seq the writer CONSUMED without a record (a crash between a bitmap write
and its record) makes the stop rule read "not written yet" for ever; after `ROOT_LEDGER_SLOTS` = 32
STOPPED polls the reader reads the whole ledger once (`meta_kv_revalidate_gap_scans`) — "a truly idle
writer costs one 128 KiB read per 32 idle polls". The reader's native watermark floors on the
adopted record's `next_ino` (:509–515, `fetch_max`). Z/: `meta/ledger.zig`'s v4 record is 32 slots
(0003 KEEP list) — the same arithmetic; the reader's poll is a `meta/revalidate.zig` over the
node-cache epoch step the port already has (`epoch_core.ReaderGate`).

### 1.12 Deadlines, lease verdicts, the departure sweep

- The recall DEADLINE on the token lane is `recall_deadline_from(explicit, None, T_owner)` = **the
  membership lease TTL** (45 s) — `ConfigMode::LiveTokens` passes no p99 evidence and `valve: false`
  (`tokens.rs:1168–1179`, :849–864; "a process-wide evidence read made one suite's fast acks the next
  suite's 1 ms deadline"); the derivation's other arm (`4 × p99 + RECALL_DELIVERY_TERM` 200 ms, floored
  1 ms, ceilinged at the TTL, :797–822) is the S10 lane's and dead here (§7).
- `LeaseVerdict` (`token_plane.rs:451–463`, :851–874): `Live` (deadline ahead of the owner's clock),
  `Expired` (past it — OR not listed: "any holder the owner does not list has LEFT or been EVICTED"),
  `Unknown` (no owner installed — the in-process contracts: waited for like live, expiry counted with
  the lease). `lease_remaining` (:880–889) bounds the pass's park so a dead reader's recall completes
  AT its expiry, never a tick later.
- **The departure sweep** (`register_holder` + `note_member_departed`, :676–705; `sweep_departed`
  :1467–1483): a member the owner EVICTS or that LEAVES has its grants retired at that instant on
  every holder — outstanding recalls complete as `expired_with_lease`, every other grant is
  `dlm_token_lease_swept_grants`; the id leaves `TOKEN_CLIENTS` so the registry stays bounded by the
  census.
- **The slot transfer's recall** (`recall_slot` :1495–1504): flush-then-transfer's token half — every
  outstanding token on an object of the departing slot is recalled and waited for exactly as a
  commit's, then settled at once; "from the tree-0 write on, this plane answers `NotHolder`".

---------------------------------------------------------------------------
## 2. Cross-owner namespace ops — ONE intent, foreign steps shipped
---------------------------------------------------------------------------

### 2.1 The intent record — v4 byte-exact

Reference (`crossvol_tx.rs:1396–1418, 1697–1898`): key = the xattr key `(intent_ino, tx_id & HASH56_MAX,
tx_id >> 56)` — "the id IS the key (56 bits in the hash field, 8 in the collision field — the full
64 bits, injectively)", so writing one needs NO collision-chain probe and NO new lock (:1390–1396);
`intent_ino` = local 0 of the initiator's SLOT namespace (`intent_ino_for_slot` :1439–1445: the native
slot's is 0, a guest slot's `guest_local_ino(s−1, 0)` — "guest cursors mint from 2, so no inode ever
owns it"); name `xtx:{tx_id:016x}` (:1449–1451, offline-probe readable). Value: header 32 B `{magic
"SQZXTX01", version u16 = 1, op u8, flags u8, tx_id u64, step_count u16, reserved u16, xxh3 u64 @24}`
+ variable steps (`kind u8` + LE fields + `name_len u8 ‖ name`), checksum over the image with the field
zeroed, decode refuses a wrong magic/version/checksum, 0 or > `XV_MAX_STEPS` steps, an unknown op or
kind, and TRAILING BYTES (:1891–1896). `XV_MAX_STEPS = 2 × MINT_SPREAD + 8 = 136` (:1408–1418 — the
one-intent `rmdir` of a K = 64 directory: parent dentry + K+2 markers + K+1 counts; "≈ 5 KiB encoded at
the widest, inside the KV value cap"). `tx_id = writer_term << 40 | seq` (:2092–2094), `seq` a
process-monotone CAS word seeded from the wall clock (:2070–2090).

v4 (`Z/src/meta/intent.zig`): the key law and the home law are kept verbatim; the value is

```
IntentHeaderWire (32 B, LE):  magic [8]u8 = "SQZXTENT", version u16 = 1, op u8, flags u8 (= 0, checked),
                              tx_id u64, step_count u16 (1..=136), reserved u16 (= 0, CHECKED), xxh3 u64
                              (over header ‖ steps with this field zeroed — core/checksum.zig)
XvStepWire (48 B, LE):        kind u8, parent_update u8, name_len u8, flags u8, ft_bits u32,
                              a u64, b u64, c u64, w0 u32, w1 u32, w2 u32, w3 u32
                              ‖ name[name_len] padded to 8 B (dentry kinds only; else name_len = 0)
  kind 1 RemoveDentry: a = parent (GLOBAL), b = expect_child (GLOBAL), name            — others 0
  kind 2 InsertDentry: a = parent, b = child, ft_bits = S_IFMT bits, name              — others 0
  kind 3 SetNlink:     a = ino, w0 = pre, w1 = post, flags bit0 = has_ctime, c = ctime  — others 0
  kind 4 TouchCtime:   a = ino, c = ctime                                               — others 0
  kind 5 MintInode:    a = ino, w0 = mode, w1 = uid, w2 = gid, w3 = rdev                — others 0
  kind 6 CreateInode:  a = ino, b = size, c = ts_ns, w0 = mode, w1 = uid, w2 = gid, w3 = rdev
  parent_update: 0 None, 1 SharedTimes, 2 ExclusiveTimes, 3 ExclusiveTimesBump (crossvol_tx.rs:1595–1602)
  op (header):   1 Link, 2 Unlink, 3 Rmdir, 4 Rename, 5 Exchange, 6 Create, 7 StripeDir (:1457–1472)
```

Every unused word of a step is checked zero; `encode` refuses a value past the volume's record cap
(`min(64 KiB, node_size/4)`, charter §5.1 — the 136-step rmdir plan encodes to ≈ 8.6 KiB, inside the
16 KiB cap at 64 KiB nodes); `decode` is total under `std.testing.fuzz` (encode∘decode = id, every
other length refused — the reference's `intent_record_round_trips_and_never_panics_on_hostile_bytes`,
`crossvol_tx_tests.rs:818`). Steps carry GLOBAL inos "so a plan survives a slot migration between
crash and recovery — the participant is resolved by routing at apply time" (:1492–1495, :2096–2103).

### 2.2 Step kinds, the witness table, the ONE applier

`xv_apply_step(step, rider, guards, served)` (`backend.rs:28047–28246`) is the ONE applier both the
live path and every roll-forward call — "idempotent is a property of one code path rather than a
claim about two" (`crossvol_tx.rs:51–67`). ONE whole-tx entry per step, the intent RIDER staged
beside it (`stage_intent_rider` :27996–28013 — `Put` rides step 0, `Delete` is the retirement; "the
rider is committed even when the witness declines the effect"); `tx.served_step = served` feeds the
door's holder-op note (the dominance window).

| Step | Witness (read under the step's guards) | Already applied | Foreign (skipped LOUD) |
|---|---|---|---|
| `RemoveDentry{parent, name, expect_child, parent_update}` | the dentry names `expect_child` (:28068–28080) | absent | names another ino |
| `InsertDentry{parent, name, child, ft_bits, parent_update}` | the name is free (:28082–28104) | present → `child` | present → other |
| `SetNlink{ino, pre, post, ctime?}` | `nlink == pre` CAS; `ctime` bumps MONOTONE over the folded view (:28106–28154) | `nlink == post ∧ pre ≠ post` (post-image answered) | neither; no record |
| `TouchCtime{ino, ctime}` | — (a Δctime merge record that only moves forward, :28155–28167) | — | no record |
| `MintInode{ino, mode, uid, gid, rdev}` (the `RENAME_WHITEOUT` char-0:0) | record absent (:28168–28198) | present | — |
| `CreateInode{ino, mode, uid, gid, rdev, size, ts_ns}` | record absent; times = `ts_ns` so a replayed apply answers the same times (:28199–28235) | present → the stored record ("never EEXIST") | — |

Parent updates (`stage_routed_parent_update` :28252–): `SharedTimes` = the Δtime merge under the
SHARED parent guard (Z/ `economy.SharedTimes`); `ExclusiveTimes[Bump]` = the exclusive RMW with the
`nlink ± 1` for a directory child. Outcome `{status: Applied | AlreadyApplied | ForeignSkipped,
inode: ?InodeValue}` (:1993–2009); the wire code `0/1/2` (:3609–3615).

**The witness is read AT THE HOLDER** (`read_inode_witness`, `backend.rs:27950–27982`; design :604):
through `token_serve` for a slot another appender leases (one grant — already held from the `lookup`
an `rm` runs first, recalled by the very step the plan then ships); the local record for an own /
maintained slot. A local `None` on a slot a LIVE foreign appender leases is REFUSED `EAGAIN`
(`xv_cross_owner_witness_refusals` — "a projection's `None` is not a witness"); the dangling-name arm
(remove the name, drop the count step) is `xv_cross_owner_dangling_names`, must-stay-0 on an armed
mount (:27936–27949 — 430 / 512 inodes orphaned in one `rm -rf` on the box before it).

### 2.3 The protocol — step 0 + barrier, dispatch, retire

`execute(routed, plan, guards)` (`crossvol_tx.rs:2791–3055`), under the op's ALREADY-HELD 4a set
(§2.4 — "this machinery acquires nothing, which is exactly why it adds no wait-for edge", :2788–2790):

1. Localise every step (`localise` :2104–2212); `coord` = step 0's volume; every participant volume
   enabled (:2797–2807). `tx_id` minted; the image encoded (:2819–2825).
2. **Step 0 + the intent**: when step 0 is LOCAL the `Put` rider rides step 0's entry — "the intent can
   never disagree with the first effect, and it costs no extra entry" (:26–32, :2826–2840, :2885);
   when step 0 is another appender's the intent is written ALONE first into the mount's own rotor
   slot (`intent_ino_for` :2753–2773; `xv_write_intent` `crossvol_arms.rs:179–186`) — "the initiator's
   ring is what the roll-forward recovers, so nothing ships before the intent is durable there".
   The intent is registered IN FLIGHT before the record exists (`note_intent_in_flight` :2846 — a
   cadence scan that sees the record always finds its owner); counted `minted` once durable
   (`note_intent_durable` :2863, :2911).
3. **Barrier** the initiator's ring after the intent-bearing entry (:2866–2871, :2978–2992 — "cross-
   DEVICE ordering: the intent must be durable before any later participant's entry is submitted";
   a strict volume barriers inside every commit so `barrier` is a no-op there, :3217–3229).
4. **Steps in plan order** (:2873–2993) through `apply_or_ship_step_retrying` (§2.9): own slot ⇒ the
   local applier; foreign ⇒ `ship_step` = ONE `MetaCall::XvStep{tx_id, step_idx, step, scope}` to the
   holder (:2444–2505; `xv_cross_owner_steps_shipped`; `ship_rtt` phase) — "the holder's reply follows
   its durability lane" (design :577). The outcome is classified by the `Dispatch` the attempt TOOK
   (`Local | Shipped`, :2520–2526), never by re-resolving the home after the fact (PR 13e F-R4: a slot
   that moved to the initiator during the ship read `Local` after it, and a holder's witness refusal
   passed as a local skip — "the create ACKED with its name nowhere").
5. **A LIVE witness refusal at a SHIPPED step** (`ForeignSkipped ∧ armed ∧ shipped`, :2916–2935): the
   plan STOPS, the applied steps before it are COMPENSATED (§2.7), the op answers the step's errno
   (`foreign_skipped_errno` :2778–2786: insert/mint → `EEXIST`, remove → `ENOENT`, count/ctime →
   `ESTALE`).
6. **A ship that fails leaves the intent OPEN, never fail-stops** (:123–130, :2957–2971): the op returns
   the error (EAGAIN-class), `note_intent_abandoned`, the cadence completes it. A LOCAL step's device
   error is the S3.5 lattice (`escalate_midplan` :3235–3257: every participant volume latched
   `disabled`, `crossvol_tx_midplan_escalations` must-stay-0) — EXCEPT a local `SlotBusy` (the slot
   moved to another appender at the door, defect 35), which is the retryable class whatever the
   dispatch (:2951–2957).
7. Every LOCAL participant volume other than the coordinator barriered before the retirement
   (:2995–3017 — "otherwise a power cut could keep the retirement and lose a step. A shipped step's
   durability is its holder's lane").
8. **Retire** — SYNCHRONOUS, before the op releases its guards (`retire` :3061–3085; `xv_retire_intent_
   at` `crossvol_arms.rs:191–207`: a `Delete` of the exact key, idempotent — an intent already gone
   writes nothing): "an intent that outlived its transaction could meet a later, legitimate mutation
   of the same objects, and recovery's witnesses assume no such interleaving exists".

Phases `xv_cross_owner_phase_ns` = `plan / intent_barrier / ship_rtt / retire / total / guard_rtt /
local_steps` (:377–408): `plan + intent_barrier + Σ ship_rtt + local_steps + retire ≈ total`;
`guard_rtt` is taken in the arm BEFORE `total` starts. Measured (`.benchmarks/…pr6-cross-owner.md`
§4, debug, loopback): create into a foreign directory **863 µs/op** (200 ops, 200 intents, 200
steps), directory rename across holders **2 659 µs/op** (400 steps, 200 lock takes); means `plan`
38 µs · `intent_barrier` 625 µs · `ship_rtt` 83 µs/step · `retire` 21 µs · `total` 837 µs; the S8
wire beneath: `rtt` 72 µs, owner `execute` 30 µs; `dir_rename_lock_wait_ns` mean 537 µs
(uncontended — the take IS one ring-0 control entry + barrier). "The create's 863 µs is 72 % the
initiator ring's barrier on a file on a CoW host filesystem … on the box the barrier term collapses
and the per-verb wire cost (≈ 80 µs loopback, ≈ 100 µs at the fabric RTT) is the row."

### 2.4 Travelling guards — `XvGuards` / `XvRelease`, ascending appender id

Two laws (`crossvol_tx.rs:822–850`): (1) **one table, one canonical `lock_many`** — every key whose
lock lives in THIS process's table rides the one stripe-canonical acquisition; a key in another
process's table is one `XvGuards` to that holder, served by the same `lock_many` in ITS table and
PARKED under the op's scope; (2) **tables in ascending appender-id order** — "a waiter for table `T`
holds only tables `< T` and the wait-for graph over tables is acyclic". A served STEP therefore never
parks on a 4a guard: under a scope that COVERS its stripes it applies with the parked guards
(`ServeGuards::Covered`), else it takes the step's keys (`Take`) (:1005–1039; coverage is by
`(volume, class, stripe)` SET — "a scope that parked unrelated keys never lets a step apply
unguarded", Issue 8b). Found by the first scoping row: the holder's `insert_dentry` waited behind a
dentry-class stripe the initiator held across the ship — the 27th of 200 directory renames stalled
for two 10 s wire timeouts (`.benchmarks/…pr6` §3); across two nodes it is the two-mutual-initiator
cycle.

`acquire_guards_leased(routed, v_idx, &scope, inos, dents, discovery)` (:1116–1160, the once-form
:1173–1355): the scope minted on the FIRST armed acquisition (`mint_guard_scope` = `seq & 2^40−1 |
2^40`, :889–891 — nonzero; 0 on the wire = "no scope travelled"); keys partitioned local / per-holder
(`table_of` :1214–1228 — an OWN REGION's slot shares this table); `discovery = true` (unlink's first
phase) acquires NO foreign table (:1232, :1246 — "the op pays ONE round trip per holder, at its full
acquisition", Issue 13); the local table taken when `own_id < holder` for the first remote table past
it (:1282–1286, :1350–1353); each remote set is ONE `XvGuards{scope, inodes: [(global, exclusive)],
dentries: [(global parent, name, exclusive)]}` (:1298–1305, `wire.rs:432–436`; `xv_cross_owner_guard_
rpcs`; `guard_rtt`); a refused/timed-out ask gets a fire-and-forget `XvRelease` behind it (:1333–1340);
the returned set carries a scope MARKER guard (`register_local_scope` :904–909 — `LOCAL_SCOPES[scope]`
lives exactly as long as the guard set) and one external guard per remote table whose drop spawns
`XvRelease{scope, ino}` (:1345–1348, `spawn_scope_release` :1358–1384); the holder's lease-expiry
sweep is the backstop (`sweep_expired_guards` :984–1002 — a scope is kept while its client is a LIVE
member, released the sweep after the owner evicts it; a client the owner does not know keeps the grace
window `stuck_grace_ms` = `T_owner` as the belt; `xv_cross_owner_guard_expiries` must-stay-0). A holder
answering `StaleHolderView` (a key whose slot it no longer leases) makes the initiator re-resolve every
key's slot at the manager and retry ≤ 2 (`STALE_HOLDER_RETRIES`, :1138–1159, `xv_cross_owner_guard_
stale_reresolves`). The served `XvGuards` skips a stripe the scope already parked (a second call under
one scope must never wait behind itself) and both guard verbs ride the dedup window ("a RESEND must
be answered from the winner's outcome", `wire.rs:330–335`).

Z/: `dlm.Manager.lockMany` is the table; `GuardSet` grows an `external` guard (`scope`, a release
closure) and the `max_requests = 16` plan bound (`dlm.zig:148–151`) is raised for the striped
`rmdir` (§4.7: `I{parent}` + `I{D}` + K `I{D_i}` + K+3 `D{}` ⇒ ≥ 2 × 64 + 4) — the plan buffer becomes
`2 × MINT_SPREAD + 8` entries, the same bound as the intent's steps.

### 2.5 The foreign steps per op

Every arm: plan under the op's guards (travelling), the witness AT THE HOLDER, one plan, `execute`.
Parent-update codes as in §2.1. `now` = the initiator's `now_ns`.

| Op | Plan (step 0 first; `@` = the slot that homes it) | Guards that travel | Witness per step |
|---|---|---|---|
| `create`/`mkdir`/`mknod`/`symlink` under a FOREIGN directory (`mod.rs:4319–4403`) | `[CreateInode{child @ creator's ROTOR, mode(+setgid from the parent), uid, gid, rdev, size, ts_ns}, InsertDentry{D, name, child, ft, Bump\|Shared}]` — `tx0` = the child record + the intent in the creator's ring | `D{D, name}` + `I{D}` (shared/excl. per the stager) at `D`'s holder | step 1: the name free (the EEXIST probe read through the divert first, :4335–4344); a dying parent (`nlink 0`) is a WITNESS refusal ⇒ `ENOENT` (§2.6) |
| `unlink`/`rmdir` of a foreign-minted child in MY directory, or of any name in a FOREIGN directory (:5552–5618) | `[RemoveDentry{P, name, expect_child, Shared\|ExclTimes\|Bump}, SetNlink{child, pre: nlink, post: is_dir ? 0 : nlink−1, ctime: now}]` — the name FIRST ("the half the caller is told about"); no count step when the child has NO record (the dangling arm) | `I{P}` + `D{P,name}` at `P`'s holder; `I{child}` at the child's holder (the two-phase: discover under the local set, drop, re-lock the FULL set, revalidate — `dlm.zig:27–30`) | step 0: the dentry names `expect_child`; step 1: `nlink == pre` |
| `link` into a foreign directory / of a foreign file (:1812–1856) | `[SetNlink{ino, pre, post: pre+1, ctime: now}, InsertDentry{P, name, ino, ft, ExclTimes}]` — the count FIRST ("the half that can refuse (EMLINK)"; `pre.nlink ≥ 65000` ⇒ EMLINK before any step) | `I{ino}`, `I{P}` + `D{P,name}` | the reply is served from step 0's post-image (`done.inode(0)`) |
| `rename` across holders, file or directory (:4846–4951) | `[SetNlink{dest, pre, post: dest_is_dir ? 0 : nlink−1, ctime}, RemoveDentry{newP, newname, dest}]` iff a destination exists (ENOTEMPTY probed BEFORE any step), then `RemoveDentry{oldP, oldname, child, Excl}`, `InsertDentry{newP, newname, child, ft, Excl}`, `TouchCtime{child, now}`, then `RENAME_WHITEOUT`: `MintInode{w @ oldP's mint slot, S_IFCHR 0:0}` + `InsertDentry{oldP, oldname, w, S_IFCHR}` | `I{oldP}`, `I{newP}`, `D{oldP,oldname}`, `D{newP,newname}`, `I{child}`, `I{dest}` | the destination's witness at ITS holder; a `ForeignSkipped` on the INSERT after the remove applied is compensated (§2.7) |
| `RENAME_EXCHANGE` (:4726) | the two removes + two inserts + two `TouchCtime`, both parents' updates | the same | — |
| directory-source `rename` of any shape | the plan above UNDER the set-wide `dir_rename` lease (§2.10) | + 1 lease RPC + `d` exact lookups | — |

A locally dispatched namespace op whose commit door answers `SlotBusy` (the slot read unleased or
ours at the plan, another appender held it at the door) is re-dispatched ONCE through the
cross-owner arm (`redispatch_once_on_slot_moved`, `mod.rs:5060–5069, 5154–5166`; `xv_cross_owner_
op_slot_moved_redispatches`, defect 30). A create into a striped parent keys the STRIPE (§4.5). The
reference keeps the rung-5 two-entry ordered split only as its pre-symmetric flat recipe
(`crossvol_tx.rs:1–47` "S3.5"); in Z/ the symmetric plane is the default format, so the intent IS the
cross-member transaction and `KvTx.cross` goes.

### 2.6 The served side — `xv_serve_step` under the holder's door

`RoutedMetaBackend::xv_serve_step(tx_id, step_idx, step, scope)` (`mod.rs:1892–2074`): localise;
**the lease check FIRST** — a slot this mount does not lease is the typed `SlotMoved{slot, holder}`
(:1902–1936, "never an errno the op surfaces"); the park seam; **the wire `child` screen** for an
insert (`screen_insert_child` :2106–: the child must have a record on its volume OR live in a slot tree
0 says SOME appender leases — "anything else is a dangling dentry a buggy peer would plant", refused
`EINVAL` + `xv_cross_owner_steps_rejected` must-stay-0; a JOINED holder's projection refreshed once
when it says `Unleased`); the guard verdict (`serve_guards_for` → `Covered` takes nothing, `Take` =
`lock_many(step_guard_keys)`, :1949–1964); for an insert **`refuse_dying_parent`** read UNDER the
guards through the divert (:1976–2010 — a parent whose record reads `nlink 0` or has no record ⇒
`ForeignSkipped` so "a live plan stops and compensates, a roll-forward retires the intent instead of
erroring at every cadence pass"; the lease re-checked AFTER the read: a release that landed under it
is `SlotMoved`); the creator census for a striped flip (`note_served_insert`, §4.3); the apply with
`served = true`; a served DIRECTORY insert re-feeds the parent memo (:2023–2042); the step counted at
the HOLDER (`XV_STEPS_*`, `xv_cross_owner_steps_served`); the dominance window fed unless the parent
is a striping domain (:2050–2072 — "a ship into a STRIPED directory or one of its stripes … feeds no
window"). Z/: the served step enters `KvFs` BELOW the level-1 op guard as its own verb, takes the
commit door like any local op (charter §5.5 "applied by ONE applier under the holder's door"), and
its tx carries `served_step = true`.

### 2.7 The live-refusal compensation

`compensate_live_refusal` (`crossvol_tx.rs:3112–3214`): every step APPLIED before the refusing one,
in REVERSE, under the op's still-held guards, through the SAME applier: `SetNlink{post → pre, ctime:
None}`; a removed dentry re-inserted with the child's type read AT ITS HOLDER (:3151–3171 — "a
foreign-minted child read off the projection would answer no record and no inverse"); an applied
insert removed; a minted child destroyed (`xv_destroy_unnamed`, `crossvol_arms.rs:227–244` — "a fresh
mint has no xattr, no layout, no block"); `TouchCtime` is monotone and stands. **The retirement RIDES
the last inverse's entry when that inverse is local to the intent's volume** (:3131–3143, Issue 26 —
compensation and retirement ONE commit: "the plan is either open-and-forward or gone"). The stated
crash window (:3104–3111): a kill between an applied step and the last inverse leaves the intent open
and the roll-forward applies the FORWARD plan, re-meeting the refusal — the C9/C10 census classes;
an abort marker on the record is the recovery-side answer the rung does not build (Z/ should: a
`flags` bit `ABORTING` on the header makes the roll-forward run the INVERSE plan — §7 Q6).

**Port (0003 §U.4): the abandonment attests, the compensation rebases.** An intent left open by a
ship-class failure at step `i` (ShipFailed, SlotMoved, Deferred, GuardRefused) rewrites its record with
the header's `applied` word = `i` (the op held every earlier step's guards, so steps `0..i` are its
own) and barriers it (`xv_applied_attested`); the roll-forward takes those steps as applied without
re-dispatching them (a later op may have moved their objects on). A compensation rebases every applied
`SetNlink`'s witness onto the count it finds under the plan's guards (`post` = the count now, `pre` =
now − the step's delta) and writes the ABORTING record with the rebased witness, so the inverse undoes
exactly this intent's delta, once, whatever a retry or another link did meanwhile. Two classes stay
undecidable and keep the leak direction (a count too HIGH, fsck C10 reports): an initiator crash
before the attestation, and the failing step `i` itself when a later op made the SAME effect (a retry
linking the same file to the same name: the dentry's witness cannot tell whose insert it is).

### 2.8 Roll-forward: at mount, the cadence, the register, the closure

- **Recovery at mount, before the mount serves** (`recover_open_intents` :3348–3365; `scan_open`
  :3282–3332): every intent HOME (ino 0 + each slot namespace's local 0, `xv_intent_homes`
  `crossvol_arms.rs:282–292`; one bounded range scan per home, `xv_scan_intents_homed` :297–318),
  SCOPED to the homes whose slot is `Local` here — "an intent in a slot another appender leases is
  that appender's to complete" (Issue 21); a JOINED appender adopts only intents in slots it LEASES
  (:3288–3303 — unleased slots are the manager's, KD-SYM-2/3); an undecodable or id-mismatched record
  REFUSES THE MOUNT (:3304–3320); ids sorted ascending.
- **`recover_one`** (:3418–3545): acquire the plan's guards through the initiator's own acquisition
  (`acquire_plan_guards` :3554–3583 — ascending volume index, `I` before `D`, travelling scopes; a
  holder unreachable for the guards leaves the intent open, never a failed mount, :3428–3439); RE-READ
  the record UNDER the guards (:3443–3463 — gone ⇒ retired meanwhile, `forget_intent`, no ghost; a
  changed step count is corruption); apply every step via the retrying dispatcher (an unreachable
  holder or a slot-moved local step past the bound ⇒ left open for the next pass, :3486–3509); barrier
  every LOCAL participant + the host; retire; `XV_TX_RECOVERED`.
- **The cadence** (`spawn_roll_forward_cadence` :3627–3644; tick = the checkpoint LANDING ceiling,
  `mod.rs:459–462` — ≈ 1.1 s): every tick `sweep_expired_guards()`; a pass runs only while the
  register holds an ABANDONED intent (`any_abandoned`); adoption by REGISTER STATE never by elapsed
  time (:3366–3378, Issue 4 — "a live op's intent is its own however long it runs"); the register is
  reconciled against the durable scan (`forget_abandoned_absent` :593–608 — an abandoned entry no scan
  lists was retired by another appender or moved with its slot, "never a ghost on
  `xv_cross_owner_intents_{open,stuck}`").
- **The register** `OPEN_INTENTS: tx_id → {since, state: InFlight | Abandoned, counted}` (:446–616):
  `minted` counts once the record is durable (or at adoption); `retired` on every removal of a
  counted entry; `open` = the population; `stuck` = abandoned entries older than `stuck_grace_ms` =
  `T_owner` (45 s — "a slot's dead holder is evicted and its slot recovered inside it", :556–566) —
  **`xv_cross_owner_intents_stuck` must-stay-0**. Closure (design :1009): `minted ≡ retired + open` per
  process. Z/: the register is a field of the set (no process static), a `std.AutoHashMap` under a
  `sync.Mutex`, sized once.

### 2.9 Re-dispatch on `SlotMoved`; the late guards

`apply_or_ship_step_retrying` (:2673–2723, `SLOT_MOVED_RETRIES = 2`): a shipped step's holder (defect
29: the manager's dominance rule handed the slot to the INITIATOR mid-plan and its door answered
`SlotBusy{slot, holder: the initiator}`; the initiator took it for a local device error and
FAIL-STOPPED both volumes) OR this mount's own door (defect 35) answering the slot-moved class ⇒
`reresolve_slot_holder(slot)` at the manager and dispatch again — locally when ours now, to the new
holder otherwise; a third stale answer is the retryable class the caller sees (the intent stays
open). `xv_cross_owner_step_slot_moved_retries`. A stale writer ERA refusal (`StaleOwnerEra`, the
holder failed over) is retried ONCE with the SAME request ids after the lane relearned the era
(`ship_ops_relearning_era` :2419–2441; `xv_cross_owner_step_stale_era_retries`).

**A step whose slot moved TO the initiator mid-plan** (`apply_step_locally` :2558–2651, PR 13b): its
guards travelled to the OLD holder, so the local apply takes the step's OWN guards in the
NON-PARKING canonical form (`dlm.try_lock_many` — Z/ `dlm.Manager.tryLockMany`, "a parking acquire
while the op holds its other guards could cycle with a peer's canonical set"), holds them across the
apply, and answers a contended stripe with `SlotMoved` (the bounded re-dispatch, then the intent stays
open for the cadence) — never an unguarded apply. A FRESH MINT is exempt ("nothing names it until the
op's own dentry step lands", :1965–1971). `xv_cross_owner_step_late_guards` / `_late_guard_refusals`.

### 2.10 The set-wide `dir_rename` lease and the exact ancestry walk

- **Every rename whose SOURCE is a directory takes it — same-slot renames too** (design :609; `mod.rs:
  5119–5184`): OUTERMOST (decided on an unguarded read of the source's type; `rename_body` re-decides
  under the guards and hands back `Ok(false)` when the source became a directory — the wrapper takes
  the lease and re-enters), held for the op, released on every exit. File renames never take it.
- **The record** (`slot_state.rs:955–1021`): tree 0 of VOLUME 0, key `b"dir_rename"`, value `{version
  u8 = 1 ‖ holder u32 ‖ term u64 ‖ since_ns u64}` = 21 B; "the expiry law is the holder's membership
  lease, never a TTL on the record". v4: `DirRenameRecordWire extern struct {version u16 = 1, reserved
  u16 (= 0), holder u32, term u64, since_ns u64}` = 24 B, total decode.
- **The verbs** (`crossvol_arms.rs:341–474`): `DirRenameLock{acquire}` → `Locked{already}` when the
  caller holds it (`manager_verb_replays`), `Busy{holder}` otherwise ("the caller WAITS, never a
  refusal — a set renames one directory at a time by design"), else ONE ring-0 control entry
  (`EntryAdmission::Try` — a full window answers `Deferred`); `DirRenameUnlock` deletes the holder's
  record (`already` when free) and wakes the in-process parkers (`DIR_RENAME_RELEASED`). **The screen**
  (`screen_dir_rename_words` :74–105, pure, fuzzed): volume 0 only, a `Live` non-own appender id, an
  unlock naming the HOLDER — else `Rejected` + `manager_verb_rejected`. **One identity's directory
  renames SERIALIZE** under a one-permit semaphore per `(volume uuid, identity)` (:29–54, Issues 23/24
  — "so the law never leans on the kernel's `s_vfs_rename_mutex`, which an S8-served `Rename` or an
  offline caller never passes through"). A dropped lease future spawns its own release (:135–154); a
  DEAD holder's record is released by the recovery driver off the death ledger (:476–480).
- **The ancestor check on EXACT data** (`refuse_rename_into_own_subtree`, `mod.rs:2391–2453`): under
  the lease, walk `new_parent`'s chain to the root refusing `EINVAL` if `moved` is on it; each hop's
  link `(p, name) → cur` from the parent MEMO (O(1)) or, on a miss, the reverse dentry scan
  (`dir_rename_parent_scans`), CONFIRMED at `p`'s slot holder through a GUARD-FREE exact lookup
  (`lookup_exact` `crossvol_tx.rs:2328–2378`: own slot ⇒ `lookup_dentry_exact_unguarded` — "a shared
  guard here could park behind the initiator's own exclusive one on a stripe collision" (Issue 2);
  foreign ⇒ `MetaCall::LookupExact{parent, name}` → `DentryExact(?(ino, ft))`, one RPC per link,
  `wire.rs:424–427, 720`); a denied/absent link re-reads after one landing ceiling, ≤ 3 scans
  (`EAGAIN` past the bound, never a guess); > 4096 hops = a cycle already exists ⇒ `EINVAL`. A stripe
  on the chain continues to its directory (:5100–5102). Pin: `two_nodes_cannot_rename_directories_
  into_a_cycle` (`sym_cross_owner_tests.rs:1086`). `dir_rename_lock_{acquires, wait_ns}`.

### 2.11 Counters of the family (reference → Z/ name, kind)

| Reference | Kind | Z/ |
|---|---|---|
| `xv_cross_owner_intents_minted / _retired / _open` (gauge) / `_stuck` (gauge, must-stay-0) | counters + two gauges | `xv_cross_owner_intents_{minted,retired}` `ShardedCounter`; `_open` `Gauge`; `_stuck` `Gauge` + Tripwire `xv_cross_owner_intent_stuck` |
| `xv_cross_owner_steps_shipped / _served / _rejected` (must-stay-0) | counters | same; `_rejected` a Tripwire site |
| `xv_cross_owner_phase_ns {plan, intent_barrier, ship_rtt, retire, total, guard_rtt, local_steps}` | histograms | ONE `ShardedLatencyHistogram` per leg; exact-sum law `plan + intent_barrier + Σship_rtt + local_steps + retire ≡ total` (the reference's `≈` becomes `≡` by stamping `total` from the same instants) |
| `xv_cross_owner_guard_rpcs / _guard_stale_reresolves / _guards_parked / _guard_expiries` (must-stay-0) | counters | same; `_guard_expiries` Tripwire |
| `xv_cross_owner_step_slot_moved_retries / _step_stale_era_retries / _step_late_guards / _step_late_guard_refusals / _op_slot_moved_redispatches` | counters | same |
| `xv_cross_owner_dangling_names` (must-stay-0 armed) / `_witness_refusals` | counters | Tripwire / counter |
| `crossvol_tx_{started, completed, recovered, steps_applied, steps_already_applied, steps_foreign_skipped (must-stay-0), midplan_escalations (must-stay-0)}` | counters | `xv_{started,completed,recovered,steps_applied,steps_already_applied}`; `steps_foreign_skipped` and `midplan_escalations` Tripwires |
| `dir_rename_lock_acquires / dir_rename_lock_wait_ns / dir_rename_parent_scans` | counter / histogram / counter | same |

---------------------------------------------------------------------------
## 3. Record-level mutations of a foreign-slot file — the metanode ship
---------------------------------------------------------------------------

### 3.1 The verb per mutation; the resolver

`setattr` / `setxattr` / `removexattr` of an inode whose slot ANOTHER appender leases, and the layout
publish, are each ONE shipped verb to the holder — NOT intents ("only the multi-object namespace ops
need the intent", design :568). The verbs are the S8 `MetaCall::{Setattr{ino, mode?, uid?, gid?,
size?, atime?, mtime?, ctime?}, Setxattr{ino, name, value}, Removexattr{ino, name}}` (`wire.rs:379–401`),
`mutating()` so they ride the dedup window (idempotent under `(lease_epoch, request_id)`, design :768).
The ONE resolver every record-level site asks (`record_ship.rs:295–316` `record_home`): unarmed, or
inside a served verb (this mount IS the holder), or a slot an OWN region leases ⇒ `Local`; `Foreign{
holder, endpoint}` through `step_home_bound` (tree 0's lessee, the endpoint bound on demand); an
unbound holder ⇒ the typed `HolderUnreachable{holder}` EAGAIN (`unreachable_holder` :279–290,
`record_unreachable`) — "never PR 13's interim `EREMOTE`, which named an arm that did not exist"
(:25–29; before it `chmod`/`touch` answered `ENOENT`, `setfattr` `EOPNOTSUPP`, and an append without an
fsync acked bytes that never landed — design :770).

### 3.2 The ship

`ship_record_verb(routed, ino, call)` (:343–388): resolve → `Local` ⇒ `Ok(None)` (apply here) → else
`ship_meta_call(endpoint, holder, call)` (`crossvol_tx.rs:2385–2408`) once; a `SlotMoved` refusal
re-resolves the lessee ONCE (`reresolve_slot_holder`) and dispatches again — locally when ours now
(`record_ship_redirects`); a second stale answer is the caller's retryable class. `record_ships`
counts the LOGICAL verb once. Phases `record_ship_phase_ns {resolve, rtt, total}` (:95–109).

### 3.3 The served side — under the holder's door, tokens recalled

The holder applies the verb under its own lease and commit door, in its own ring; its conveyor pass
RECALLS the object's tokens — "the writer's own included, so the writer re-fetches X at its next
resolve, exact" (design :644, :768). `note_served(routed, ino, failure, served_at)` (`record_ship.rs:
397–432`): `record_served`, or `record_refusals` (must-stay-0; a slot-moved refusal is the requester's
re-resolve, not a refusal); then the holder's OWN caches (§3.4) and the dominance window fed as the
REQUESTER's op (`note_served_slot_ship` :445–470 — "a writer that dominates a slot through its data
writes alone earns the offer", Issue 8; the same feed the served publish runs). The custody lease:
the publish presents the per-HOLDER custody lease epoch the `CustodyGrant` (§1.3 G5) established
(`publish.rs:2200–2253` `PublishTarget{peer, holder: ?WriteCustodyClient}` — `epoch()` = the holder's
grant to THIS mount; the served side's era gate and custody-scoped compose judge the publish by it).

### 3.4 The holder's invalidation of its OWN caches and kernel view

`install_served_mutation_sink` (`fuse_client.rs:17219–17262`): on a served mutation of `ino`
(1) the daemon's attr cache entry goes WHOLE ("the merge law keeps the cached size and the newer times
over a refetch — a served truncate or a backwards `touch` can never win a merge"); (2) `router.discard_
clean_layout_entry(ino)` under the dirty law — `DirtyKept` beside a RECORD verb is legal and counted
(`served_mutation_dirty_kept`), beside a served LAYOUT PUBLISH it is the tripwire `served_publish_over_
dirty_entry` ("two writers under one custody; the dirty entry is kept" — `record_ship.rs:174–198`);
`HeldSkipped` → `served_mutation_discard_skipped`; (3) the KERNEL hook (`make_served_mutation_kernel_
hook` :31368–31387): `FUSE_NOTIFY_INVAL_INODE` FIRST — attrs-only `(off −1, len 0)` for a record verb,
attrs + every page `(0, 0)` for a publish — then, iff `kernel_prune_capable()` (uapi ≥ 7.45 AND the
writeback cache negotiated, :31395–31419 — a writeback mount on an older kernel is announced ONCE as
the loud degrade), `FUSE_NOTIFY_PRUNE` of the ino: "the prune is what makes a `FUSE_WRITEBACK_CACHE`
kernel adopt a PEER mount's size / mtime / ctime … `fuse_reverse_inval_inode` never moves `i_size`";
an inode a process holds OPEN at the holder is the stated residual (the prune cannot evict it, the
invalidation drops its pages). `served_mutation_invals / _prunes`. The SAME hook from a THIRD mount: a
RECALLED object's sink (`install_recalled_object_sink` :17246–17261) invalidates the attr cache and
the kernel as `Data` (the recall does not say which). Z/: `session.notifyInvalInode(ino, -1, 0)` /
`(ino, 0, 0)` then `notifyPrune(&.{ino})` on the sideband (`session.zig:812–826`; a pre-7.45 kernel's
EINVAL counts `transport_notify_failed`); the daemon's attr cache is `KvFs`'s; the layout entry is the
plane's (`Epochs`/`LayoutLocks`).

### 3.5 The layout publish for a foreign-slot file

The data plane's publish of a file whose slot another appender leases SHIPS the layout transaction
to the holder (`publish_target` `publish.rs:2256–2280`: the S8 volume owner first, else `record_home`'s
`Foreign{holder}` with the per-holder custody client joined on first use), where the SAME
`publishLayout` (Z/ `data/layout.zig:1466` — the `Put` of the layout xattr + the `+ref/−ref` diff in
ONE tx under the holder's (3.5)/(3.7)) runs under the holder's door; the holder's pass recalls the
file's tokens — including the WRITER's — BEFORE the publish commits (§1.7: the writer's own
`layout_dirty` entry is KEPT at its recall); the terminal frees of the displaced blocks then ship to
the allocation holder "after the recall acks (no separate flush-cadence wait; the recall IS the
qualification)" (design :651). `foreign_publish_ships` counts publishes that LANDED (at the terminal
reply, once per logical publish — a slot-moved redirect re-dispatches the same publish, `publish.rs:
2217–2231`), `foreign_publish_served` at the holder, `foreign_publish_refusals` the terminal failures;
at rest `ships ≡ served` fleet-wide. Design's price (:755): "1 custody grant + 1 publish ship per
layout publish (today's S9 shape, 3.6 verbs/MiB) + the holder's recall of the file's other token
holders per publish".

---------------------------------------------------------------------------
## 4. Directory striping — DNE-2 shape, K = 64
---------------------------------------------------------------------------

### 4.1 The map: `K + 2` NUL-led dentries in `D`'s own tree

`dir_stripe.rs:17–40, 124–201`: `\0sqz.stripe:NN → D_i` (exactly TWO decimal digits — "the decoder is
the encoder's inverse and nothing wider": `\0sqz.stripe:5` and `:005` are NOT markers, :153–192),
`\0sqz.migrating → D_0`, `\0sqz.striped → D_0` **LAST = the commit marker** ("the map is live iff it
exists and names stripe 0"). **No marker names `D` itself** (:24–26, :130–135 — a self-reference was the
reverse scan's first hit and `..` of `D` answered `D`). Why dentries (:26–35): the map RIDES `D`'s
token verbatim (a grant pages a directory's dentry set — no token-API change); the flip is `K + 2`
`InsertDentry` steps of ONE intent (no new step kind, no new applier); C9's dentry pass marks every
marker's TARGET so a mapped stripe is REFERENCED. **No slot-tree key kind is added**; the reserved NUL
name space inside kind `0x02` is the bit's only addition (:35–37). A NUL-led name is unreachable from
FUSE (the VFS refuses NUL in a component) and filtered from `readdir` at the ONE pager (`is_marker_name`
:168–170; `refill_stream` :2090–2092). **A stripe never carries a map** (:37–40; `is_stripe` :1002–1007,
the reverse scan once per flip candidate; the explicit flip refuses, `flip_dir` :1137–1146).

`StripeMap{dir, stripes: [K]global ino, migrating}` (:329–348), `stripe_for(hash54) = (i, stripes[i])`
with `stripe_of(h, K) = h % max(K, 1)` (:199–201) over the SEEDED `hash54` of `D`'s volume
(`hash54_on` :638–643) — "every stripe lives on `D`'s volume so … the K-way merge has ONE `(hash54,
coll)` order and the shipped cookie is its exact continuation" (:52–57). `StripeRoute{map, index,
stripe}` (:352–357). The per-mount `StripeState` (:437–469): `maps` (a migrating map is cached and
re-kicks its migration when this mount holds `D`), the HOLDER's negative cache `unstriped: D → (holder,
g)` ("only the holder flips a directory it holds, so its own flip is the one invalidation"; a stale
lease stamp reads as no hint — Issue 26) bounded `UNSTRIPED_CACHE_MAX = W = 65 536` (:430–434), `known_
stripes`, the trigger `census`, `in_flight` (single-flight per directory), `times_persisted_ns`,
`stripe_at_mkdir`.

### 4.2 Where the stripes live; the flip as ONE intent

`flip_dir(dir, k, creators, belt)` (:1107–1161) → `flip_dir_inner` (:1163–1344), on the HOLDER of `D`
(a non-holder answers EAGAIN naming the holder, `not_holder` :579–588): refuse unarmed / knob `≤ 1` /
not a directory / already striped / a stripe; single-flight. **Phase A** — the SUPPLIED stripes: the
`K` heaviest foreign creators (the census, heaviest first, :1198–1215) are each asked
`SupplyStripeIno{dir, index, supplier}` (`wire.rs:450–454`, `MetaVerb 0x90`) to mint an `S_IFDIR` record
in THEIR slot on `D`'s volume (:1402–1409 `mint_stripe_in_slot` — ONE single-key `I{new}` acquisition
while the caller holds NO other guard; "a creator that declines or is gone is replaced by the next";
`dir_stripe_supply_rpcs` per mint, served or own). **Phase B** — the remainder's inos allocated in the
holder's slot on `D`'s volume (:1217–1225, :1370–1377). **Phase C** — the op's ONE guard set: `I{D}` +
`I{D_i}` for every stripe minted HERE + `D{D, marker}` for all `K + 2` markers, canonical and
stripe-deduped, taken AFTER the suppliers' asks and BEFORE the remainder's mints (:1226–1249; PR 10
review Issue 28: the landed build took the marker set first and each mint's `I{D_i}` as a SECOND
acquisition on the same striped table — a self-deadlock, deterministic at `SQUEEZEFS_DLM_STRIPES=1`).
**Phase D** — the remainder's records minted under the set (:1268–1286). Then ONE `XvOp::StripeDir`
intent of `K` `InsertDentry{D, "\0sqz.stripe:NN", D_i, S_IFDIR, None}` + `InsertDentry{D, migrating,
D_0}` + **`InsertDentry{D, striped, D_0, ExclusiveTimes}` LAST** (:1291–1319) — "a reader sees either
no map or the whole map, and a kill at any step rolls forward to the whole map". The negative cache is
forgotten BEFORE the write (:1320–1322 — "a plan severed after its commit marker landed must not leave
'unstriped' cached"). A failed intent names the minted stripes LOUD as C9's class (`name_unmapped_
stripes` :1351–1365). Then the lazy migration is kicked (:1155–1159). `dir_stripe_flips`,
`dir_striped_dirs`. Measured (`.benchmarks/…pr7b` §4 row 2–3): a solo flip (14 local mints + ONE intent
of 16 inserts + barrier) 6.7–7.3 ms; with 7 holders supplying 14 stripes over loopback 10.8–12.4 ms,
`dir_stripe_supply_rpcs` 14, `steps_shipped` 17.

### 4.3 Triggers

- **Automatic** (`note_served_insert` :1570–1646, fed from `xv_serve_step`'s insert arm with the
  CREATOR = the shipped child's slot holder): per directory a `DirCensus` over the `T_idle` half-window
  pair (`DominanceWindow::epoch(now, t_idle) = now / (t_idle/2)`, `slot_lease_core.rs:1238–1240`; O(1)
  increments — `cur`/`prev` `BTreeMap<creator, ops>`, `total`, `distinct`, :365–416); due iff
  `distinct ≥ 2 ∧ total > N_floor` (`flip_due` :568–571 — "a directory ONE foreign node writes is a
  handover candidate, not a striping one", design :616); `N_floor = max(2, ceil(ewma_handover_ns /
  ewma_ship_ns))`, cold start `4 × ewma_barrier + 3 × ewma_ship_rtt` (`slot_lease_core.rs:1324–1336`);
  the heaviest-first SORT runs once when due; `CENSUS_DIRS_MAX = W / MINT_SPREAD = 1 024` directories,
  least-recently-touched evicted (:419–422, :1588–1591); the flip spawns contained, after the
  `is_stripe` belt (:1602–1645). A striped directory or a stripe feeds NO census and NO dominance
  window (`is_striping_domain` :1565–1568; `mod.rs:2062–2072`).
- **Explicit**: `setfattr -n user.squeezefs.stripes -v K <dir>` — intercepted at the FUSE boundary
  BEFORE the allowlist as a COMMAND never a stored value (:116–122); `K` clamped `2..=64`, unparseable
  takes the knob's (`clamp_explicit_k` :213–218); `getfattr` answers the `K` in force. From a
  non-holder it answers EAGAIN naming the holder (design :622 item 7 — the SHIPPED explicit flip is
  owed in the reference; Z/ ships it as one `MetaCall` to the holder).
- **`-o stripe_dirs`**: every `mkdir` under an armed volume stripes at creation (`stripe_at_mkdir`,
  :462–464, :625–634), the flip run AFTER the mkdir's own guards dropped with `StripeBelt::
  KnownDirectory` (no reverse scan — Issue 24).
- Knob `SQUEEZEFS_SYM_DIR_STRIPES` int `1..=64`, default derived `MINT_SPREAD = 64`; `1` = striping OFF
  (the A/B control — "no flip ever runs"; an existing map still routes, `the_knob_at_one_never_
  stripes_and_still_routes_an_existing_map`, `sym_dir_stripe_tests.rs:2314`).

### 4.4 Lazy re-home; `D`'s tree as a permanent secondary home

- **Reads**: `stripe_locate(parent, name, route)` (:815–830) — the stripe first, then `D`'s own tree
  ("the LWW rule: the stripe wins"). `readdir` merges `D`'s own stream as stream K (§4.6).
- **Mutations key the STRIPE and re-home first**: `stripe_mutation_parent` (:857–878) — a name still in
  `D`'s tree is moved by `migrate_one` under the same guards THEN the op revalidates against the
  stripe alone (Issue 4: "a routing decision read once and never re-validated gave spurious `ENOENT`s
  racing the migration and stranded a rename's destination"); `dir_stripe_rehomed_on_touch`. A NEW
  name's EEXIST screen runs over `D`'s own tree, the insert keys the stripe (`stripe_insert_parent`
  :836–847). `rename` under a striped parent keys each side's stripe (`mod.rs:5092–5118`): a rename
  within one striped directory across two stripes is "the ordinary two-parent rename, cross-owner when
  the stripes' holders differ".
- **The background migration** (`kick_migration` :1671–1699 → `migrate_dir_inner` :1729–1789):
  `migrate_scan` pages `D`'s tree at `MIGRATION_SCAN_PAGE = 8 × 64 = 512` (:424–428) and moves every
  non-marker name (`migrate_one` :1828–1959): ONE intent `[InsertDentry @ stripe, RemoveDentry @ D]`
  under `I{D}`, `I{D_i}`, `D{D,name}`, `D{D_i,name}` (+ `I{child}` for the LWW loser's count) — "insert
  into the stripe FIRST, remove from `dir` second … a kill between the two leaves the name in both
  places and the stripe wins"; a name the stripe already holds with ANOTHER child is the LWW loser:
  a FILE loser's name goes with `SetNlink{nlink → nlink−1}` (its witness at its holder), a DIRECTORY
  loser is LEFT and named loud (C17 `unmigrated-name`) "rather than unlinked over its contents"
  (:1889–1937). `dir_stripe_migrated_names`. The `migrating` marker clears only when `D`'s tree is
  re-verified EMPTY of user names UNDER the exclusive `I{D}` + `D{D, migrating}` guard (:1745–1778,
  Issue 4c); ≤ `MIGRATION_ROUNDS_MAX = COVER_CYCLES_MAX = 64` rounds, else the flag stays and resumes at
  the next ACCESS by the holder ("a mount-time census of migrating directories was rejected as the
  census's cost", design :622 item 6). Measured: 2 000 names at 908–960 µs each (one intent + barrier
  per pair, `.benchmarks` §4 row 2).

### 4.5 Operations on a striped directory

`create/unlink/lookup(D/name)`: read `D`'s token (the map rides it, `stripe_map` :696–), compute `i`,
one op on `D_i`'s holder — the same shipped `InsertDentry`/`RemoveDentry` with a different key ino
(design :617). `lookup` takes a SHARED `D{D_i, name}` then `stripe_locate` (`mod.rs:5006–5021`).
**Every insert path refuses a DYING parent** (`refuse_dying_parent` :956–974: the key parent's `nlink`
AT ITS HOLDER (`key_parent_nlink` :924–926 → `stripe_record` :939–946 = `getattr` through the divert
— "the ONE per-stripe record read", PR 13 defect 24: the projection's root recycled under the lessee's
compaction and every `stat /` on a joiner spun the traversal budget) == 0 or no record ⇒ `ENOENT`,
`dir_stripe_dying_refusals`): create, link, rename's destination, the served step (§2.6). The
shipped insert's witness refusal at a dying stripe is the plan's `EEXIST`; the op's errno is decided
on one record read of the error path (`dying_parent_errno` :983–995 → `ENOENT`). `..` of a child of a
striped directory is the directory the stripe belongs to — one more hop on the reconnect path only
(`stripe_parent_dir` :1013–; `mod.rs:4983–4989`). A `create` with a PRESET child into a striped parent
is refused (`sym_dir_stripe_tests.rs:1845`).

### 4.6 `readdir` — the K-way merge in cookie order

`readdir_striped_page(map, offset, max)` (:1989–2060): `K + 1` streams (every stripe + `D`'s own tree),
each refilled only when its head is consumed in pages of `merge_stream_page = max(max / (K+1), 8)`
(`MERGE_STREAM_PAGE_FLOOR = 8`, :525–530, :2067–2069 — "a merged page reads `≤ max + (K + 1) × page` raw
entries whatever the stripes hold — never `K × max`"); pop the smallest head cookie; an entry from
`D`'s own stream whose stripe holds the same name is the LWW loser and skipped (the stripe's stream
refilled past that hash first, :2026–2051); the returned cookie is the exact continuation ("every
stream resumes strictly after it, so a name present for the whole scan is returned exactly once under
concurrent creates/unlinks", :1976–1983). Measured (debug, in-process, 2 000 names, K = 14): striped
`readdir` 9.6–10.0 ms vs flat 14.2–15.0 ms — "the `ls` COST the design prices is the token reader's:
`K + 1` grants on first touch, 0 RPCs until a recall"; at K = 64 a cold `ls` is 65 grants against
the flat directory's 1 (`.benchmarks/…pr7b` §4 row 2). A token READER lists a striped directory as
the merge of its stripes' tokens (`sym_coherence_tests.rs:1784`), including across a flip it held
`D`'s token through (:1882); the fleet row reads 20 000 children over 64 stripes as `K + C + 3` tokens
with 0 data-leaf reads (design :622 item 7). `dir_stripe_readdir_merges`.

### 4.7 `stat D` — the fold; `rmdir D` — ONE intent under ONE exclusive set

- **The fold** (`fold_striped_attrs` :2120–2159): `nlink = D.nlink + Σ_i max(D_i.nlink − 2, 0)`, `mtime`
  / `ctime` = max over `D` and its stripes; every stripe's record read AT ITS HOLDER; a holder that
  cannot be reached contributes NOTHING for the window (`dir_stripe_fold_unreachable`, PR 13 defect
  24's second face — "every path walk across `D` — `.stats` included — failed `EIO` for the 15 s
  between seven joiners' deaths and their recovery"). Read under the SHARED `I{D}`; the fold's times
  are PERSISTED onto `D`'s own record once per CHECKPOINT CADENCE per directory (`persist_striped_times`
  :2167–2218 — "the write recalls every reader's token on `D`, so it must never run at the `stat`
  rate"; only the holder writes; never lowers a stored time; `dir_stripe_time_batches`). Nothing ships
  per insert: "a stripe's own record IS the durable delta" (:59–68).
- **`rmdir D`** (`rmdir_striped(key_parent, name, map)` :2391–2580): ONE guard set per volume in
  ascending volume order, each canonical — `I{parent}` + `D{parent, name}`; `I{D}` + `I{D_i}` EXCLUSIVE
  on every stripe (travelling to a foreign holder) + `D{D, marker}` on every marker (:2402–2437). Then,
  having written NOTHING: the dentry revalidated (another ino ⇒ EAGAIN, gone ⇒ ENOENT), the map
  re-read from the markers and compared (changed ⇒ EAGAIN), `D`'s own tree probed for a user name
  (`page_names(D, 0, 1)` — a `max = 1` read that skips markers, :2102–2110) or `nlink > 2` ⇒ ENOTEMPTY,
  every stripe's count at its holder (`nlink ∉ {0, 2}` ⇒ ENOTEMPTY), every stripe `IsEmpty{stripe,
  scope}` (`wire.rs:458–461`, 0x91 — "under `scope`'s parked guards when they cover the stripe, else
  under the stripe's exclusive guard"; exact: "every insert that took a stripe's guard has landed,
  none can be in flight") (:2441–2497). Then ONE `XvOp::Rmdir` intent: `RemoveDentry{parent, name,
  D, Bump}`, `RemoveDentry{D, striped, D_0}` FIRST ("a reader between steps sees no map"), `{D,
  migrating}` if set, `{D, stripe:NN, D_i}` × K, `SetNlink{D_i, 2 → 0}` on every stripe with count 2
  (the DYING mark), `SetNlink{D, nlink → 0, ctime}` (:2506–2563). Hygiene after the guards drop:
  `DestroyStripe{stripe}` per stripe (`wire.rs:464–466`, 0x92 — "refused EBUSY/ENOTEMPTY otherwise;
  `Unit` for an absent record"; a stripe left is an `nlink 0` corpse named by no map — the sweep's)
  (:2565–2577). `dir_stripe_rmdirs`. Standing in the reference (design :622 item 9): the `IsEmpty` probe
  of a stripe held by a truly REMOTE appender under the served context answers under the holder's OWN
  guard, so a file create at the holder can land between the probe and the `SetNlink 2 → 0` — Z/ MUST
  make the stripes' exclusive guards travel (`XvGuards` on `I{D_i}` + `D{D_i, *}` is not expressible
  per-name; the spec's answer: the served `IsEmpty` takes the stripe's EXCLUSIVE `I{D_i}` under the
  travelling scope's park so every insert path's `I{D_i}` acquisition waits it out — the dying mark
  then lands under the same held guard).

### 4.8 fsck C17 — report-only

`C17Shape` (`fsck.rs:4570–4625`): `multiply-mapped` (a stripe named by ≥ 2 directories' maps),
`missing-stripe` (a map entry names an ino with no record), `incomplete-map` (a commit marker over
< 2 stripe entries or a gap in the indices), `misrouted-name` (a dentry in stripe `i` whose `hash54 %
K ≠ i`), `unmigrated-name` (a non-marker name in `D`'s own tree after `migrating` cleared — "a
stale-route insert's trace; reads still serve it … and its next mutation re-homes it"). Nominated on
the ONE dentry walk (a NUL-led name is a C17 nomination, :3997), confirmed against ONE fresh view per
pass (:4891–4893), verify-before-report (:6203). `fsck_stripe_findings` must-stay-0; every planted shape
is found and nothing else (`sym_dir_stripe_tests.rs:2647`).

---------------------------------------------------------------------------
## 5. Knobs, gauges, constants with WHY, measured numbers
---------------------------------------------------------------------------

### 5.1 Knobs (class per `Z/src/core/knobs.zig`)

| Knob | Kind / range | Default | Class | Reference |
|---|---|---|---|---|
| `SQUEEZEFS_SYM_DIR_STRIPES` | int `1..=64` | derived `MINT_SPREAD` = 64 (`1` = striping off) | `ab_lever` (the control arm is 1) | design :823; `dir_stripe.rs:106–115, 205–208` |
| `-o stripe_dirs` | mount option | off | mount option (the DNE-2 default lever) | design :840; `dir_stripe.rs:625–634` |
| `setfattr -n user.squeezefs.stripes -v K` | xattr command | — | not a knob (a per-directory verb) | `dir_stripe.rs:116–122` |
| `SQUEEZEFS_SYM_T_IDLE_MS` | int `1000..=600000` | derived `T_owner` | `derived_override` (the sibling spec's row; the striping trigger reads its window) | design :818 |
| `SQUEEZEFS_TEST_XV_*` (seam after N steps, serve refuse/misdeliver/slot-busy/skip/hold, cadence hold, stuck-after, guards force-remote, release hold, dir-rename identity), `SQUEEZEFS_TEST_TOKEN_*` (dispatch hold after check, custody not-holder once, install-carried fail once, conveyor hold post-recall), `SQUEEZEFS_TEST_STRIPE_{HOLD_MIGRATION, RMDIR_HOLD_MS}` | test seams | inert | `test_seam` — in the reference these are `AtomicU64/AtomicBool` statics (`crossvol_tx.rs:167–288`, `token_plane.rs:495, 508, 516`, `dir_stripe.rs:258–266`); in Z/ they are `SQUEEZEFS_TEST_*` rows read ONCE into the set's seam struct, never process statics (§7 Q4) | — |

**Not carried** (§7): `SQUEEZEFS_DLM_TOKEN_CACHE_MAX`, `SQUEEZEFS_DLM_RECALL_BATCH_MAX`, `SQUEEZEFS_DLM_
RECALL_DEADLINE_MS`, `SQUEEZEFS_DLM_RECALL_COOLDOWN_MS` (`tokens.rs:73, 758–762`), `SQUEEZEFS_FREE_GRACE_
DRAIN_EPOCH_STAMP`, `SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED` (`ro_coherence.rs:533–538, 460–470`),
`SQUEEZEFS_MW_AUTHORITY` as the reader's holder endpoint (PR 5's seam — the holder resolves off tree
0), `SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS` (0002 row 188: `T_owner` is a `pub const`). "No new knobs"
is PR 5's own row (`.benchmarks/…pr5` header).

### 5.2 Gauges / counters (reference name → Z/ kind; `*` = must-stay-0 ⇒ `counters.Tripwire` site)

**Token family, holder side** (`token_plane.rs:3963–4004`): `dlm_token_grants_served`,
`dlm_token_recalls`, `dlm_token_recall_acks`, `dlm_token_recall_expired_with_lease`, `dlm_token_lease_swept_
grants`, `dlm_token_not_holder_redirects`, `slot_resolve_dead_redirects`, `dlm_token_park_expired_refusals`,
`dlm_token_nonmember_refusals` (a counter, not must-stay-0: a frame served after its sender's deadline — 0003 §AE.3), `dlm_token_custody_rejected`*, **`dlm_token_recall_timeouts_live`***,
`dlm_token_releases`, `dlm_token_recall_batches`, `dlm_token_grant_parks`, `dlm_token_regrant_under_
recall_waits`, `dlm_token_outstanding` (gauge), `dlm_token_recall_fanout` (p50/p99 + the distribution
— `QueueDepthHistogram`), **`dlm_token_recall_rtt_ns {send, drain, ack, total}`** (exact-sum
`ShardedLatencyHistogram` ×4).
**Reader side** (:4015–4097; the FOLD over every plane the volume reads through — counters sum, the
two posture words read 1 only when EVERY channel says so): `dlm_token_grants`, `dlm_token_cached`
(gauge), `dlm_token_hits`, `dlm_token_recalls_received`, `dlm_token_recalls_acked`, `dlm_token_reader_
releases`, `dlm_token_cached_bytes` (gauge; the R5 component `dlm_token_records_bytes`), `dlm_token_
sheds`, `dlm_token_oversize_refusals`, `dlm_token_serve_refusals`, `dlm_token_channel_rounds`,
`dlm_token_fetch_retries`, `dlm_token_channel_fresh` / `_alive` (posture), `dlm_token_grant_sessions` /
`_dialed`, `dlm_token_holder_repoints`, `dlm_token_membership_waits`, `dlm_token_grant_rtt_ns`,
`dlm_token_reader_holder_planes`, `dlm_token_reader_unbound_holders`, `dlm_token_root_projection_serves`
(R-SYM-4's ONE named exception — the mount root's attrs off the local projection when the plane
answers a TRANSIENT wire class, so `.stats` stays reachable, :3728–3741), `dlm_token_reader_redirects_
followed`, `dlm_token_reader_holder_resolves`, `dlm_token_recall_purged_keys`, `_scoped_purges`,
`_census_purges`, `dlm_token_recall_dirty_kept`, `dlm_token_recall_discard_skipped`.
**The reader's serve ledger** (`ro_coherence.rs`): `reader_serves_inflight` (gauge), `reader_serve_step_
races`, `reader_serve_slot_overruns`*, `free_grace_drain_wakes`; `reader_staleness_bound_ms` = 0 (kept
for tooling); `reader_quiescent`, `reader_quiescent_writer_appeared` (posture words).
**Free-grace faces under tokens** (design :1001): `free_grace_recall_gated_frees`, `free_grace_recall_
timeout_deferrals`; `free_grace_hold_ms` ≈ the recall RTT.
**Cross-owner family**: §2.11. **Record ship** (`record_ship.rs:200–239`): `record_ships`, `record_served`,
`record_refusals`*, `record_ship_redirects`, `record_unreachable`, `foreign_publish_ships`, `foreign_
publish_served`, `foreign_publish_refusals`, `served_mutation_invals`, `served_mutation_prunes`,
`served_mutation_dirty_kept`, `served_mutation_discard_skipped`, `record_ship_phase_ns {resolve, rtt,
total}`; tripwire `served_publish_over_dirty_entry`*.
**Striping family** (`dir_stripe.rs:302–321`): `dir_striped_dirs` (gauge), `dir_stripe_flips`, `dir_
stripe_supply_rpcs`, `dir_stripe_ships`, `dir_stripe_readdir_merges`, `dir_stripe_migrated_names`,
`dir_stripe_time_batches`, `dir_stripe_dying_refusals`, `dir_stripe_rmdirs`, `dir_stripe_rehomed_on_
touch`, `dir_stripe_fold_unreachable`, `fsck_stripe_findings`*.
**Control-plane poll**: `meta_kv_revalidate_ledger_read_bytes`, `meta_kv_revalidate_ledger_full_reads`,
`meta_kv_revalidate_gap_scans`.

**Closure laws** (design :1009): `dlm_token_recalls ≡ dlm_token_recall_acks + expired_with_lease`;
`xv_cross_owner_intents_minted ≡ retired + open`; `steps_shipped ≡ steps_served` at rest fleet-wide;
`foreign_publish_ships ≡ foreign_publish_served` at rest; `dir_stripe_ships + own-stripe creates ≡
creates into striped directories`; `dlm_token_recall_rtt_ns: send + drain + ack ≡ total` per batch;
`free_grace_deferrals ≡ releases + offsets`. Every one reads 0 / `solo` on a `--single-writer` mount
(charter §5.5; `a_single_writer_volume_carries_no_token_plane`, `sym_coherence_tests.rs:4217`).

### 5.3 Constants with WHY

| Constant | Value | WHY | Ref |
|---|---|---|---|
| `K = MINT_SPREAD` | 64 | "the shipped granularity constant"; 64 stripe inos per directory is cheap; never re-striped (a rehash of every name) | design :614; `dir_stripe.rs:115` |
| `XV_MAX_STEPS` | `2 × 64 + 8 = 136` | the widest plan (the one-intent rmdir of a K = 64 directory: 1 + K+2 removes + K+1 counts) + pre-7b headroom | `crossvol_tx.rs:1408–1418` |
| `STRIPE_MARKER_DIGITS` | 2 | `STRIPES_MAX` is 64 — the codec's width IS the bound | `dir_stripe.rs:153–163` |
| `MERGE_STREAM_PAGE_FLOOR` | 8 | the shipped readdir's smallest useful page; below it per-refill overhead dominates | :525–530 |
| `MIGRATION_SCAN_PAGE` | `8 × 64 = 512` | one merge-floor page per stripe of the widest map = the KV walkers' page | :424–428 |
| `CENSUS_DIRS_MAX` | `W / MINT_SPREAD = 1 024` | one hot shared directory per rotor-sized slice of the slot namespace | :419–422 |
| `UNSTRIPED_CACHE_MAX` | `W = 65 536` | one hint per routing slot; 24 B each ≈ 1.5 MiB; flushed whole at the cap | :430–434 |
| `MIGRATION_ROUNDS_MAX` | `COVER_CYCLES_MAX = 64` | "the bound every bounded cover loop of the program shares" | :1721–1727 |
| `grant_dentry_budget` | `CONTROL_MAX_FRAME_BYTES / 2` = 512 KiB | half the frame for xattrs + dentries, half for attrs/framing/MAC — the S8 encode check's conservative split | `token_plane.rs:390–396` |
| `records_budget_bytes` | `max(mem_budget / 256, 1 MiB)` | 64 MiB at 16 GiB ≈ 1.6 M dentries / ≈ 1 000 full layouts — a reader's live interest set; the floor is one grant page | `tokens.rs:107–128` |
| `TOKEN_ENTRY_BYTES` / `DIR_RECORD_BYTES` | 128 / 24 | the entry + Arc + bucket + index header; cookie + ino + type + index slot | `token_plane.rs:2004–2007` |
| recall deadline (token lane) | `T_owner` = 45 s | the reader's LEASE TTL by derivation — never a p99 ("one suite's fast acks the next suite's 1 ms deadline"); past the TTL the fence arithmetic bounds the client anyway | `tokens.rs:849–864, 1168–1179` |
| `poll_park_bound` | `clamp(deadline/4, 100 ms, 1 s)` | a recall issued mid-round is delivered within a fraction of the deadline; an idle channel is not a busy loop | `token_plane.rs:470–480` |
| `RECALL_POLL_PARK_FLOOR` | 100 ms | the scheduling grain below which the standing poll degenerates into a busy loop | `tokens.rs:803–807` |
| the expiry tick | `max(deadline/4, 1 ms)` | bounds how late an UNKNOWN-remaining lease is re-read; acks and the earliest live expiry wake earlier | `token_plane.rs:1366–1370` |
| `channel_fresh` window | `2 × park(1 000 ms) + slack(2 000 ms)` | a recall issued while we serve is deliverable within the window (two rounds + slack) | :2453–2462; `tokens.rs:1907–1911` |
| reconnect backoff | `[50 ms, ×2, 5 s]` = `[park/20, park × 5]` | one RTT-class retry doubling to the S10 park derivation's ceiling — both derived from the park, "never free constants" | `token_plane.rs:2009–2016` |
| `SERVE_GEN_SLOTS` | 64 = `u64::BITS` | the poison mask's width — a representation bound; ≥ 64 s at one step per second per volume | `ro_coherence.rs:224–228` |
| serve shards | `next_pow2(cpus) ∈ [1, 64]` | the fuse3 `ShardedCounter` law over the daemon's divided root | :259–270 |
| `stuck_grace_ms` | `T_owner` = 45 s | a slot's dead holder is evicted and its slot recovered inside it; an intent open past it names a slot no live holder serves | `crossvol_tx.rs:556–566` |
| cadence tick | the checkpoint landing ceiling (≈ 1 100 ms) | an abandoned intent completes within a few ceilings of the holder returning | `mod.rs:459–462` |
| `SLOT_MOVED_RETRIES` / `STALE_HOLDER_RETRIES` | 2 / 2 | one legal move + one re-move; a third stale answer is the retryable class | `crossvol_tx.rs:2683, 1138` |
| ancestry re-read bound | 3 scans a ceiling apart; > 4096 hops = a cycle | a stale memo hint costs no wait; deeper than any legal tree | `mod.rs:2407–2450` |
| `N_floor` | `max(2, ceil(ewma_handover / ewma_ship))`; cold `4 × barrier + 3 × ship_rtt` | a handover must pay for itself in ships saved; 2 = a single touch never moves anything | `slot_lease_core.rs:1324–1336` |
| `ROOT_LEDGER_SLOTS` (gap belt) | 32 stopped polls | "a truly idle writer costs one 128 KiB read per 32 idle polls" | `revalidate.rs:453–472` |

### 5.4 Measured numbers (SCOPING — debug, in-process, loopback; the D1 arithmetic)

| Row | Value | Source |
|---|---|---|
| D1: `tar -x` of 2 384 entries into a directory the node created | **0 wire verbs/entry, 0 structural verbs** | design :766 |
| D1: `create`/`mkdir` under an OWN directory | 0 RPCs | design :748 |
| D1: `mkdir /jobs/X` under a STRIPED `/jobs` | 1 ship + 1 intent barrier to the holder of stripe `hash(X) % 64`; 195 ships per stripe holder per 12 500-creator wave ≈ 6 ms each (was 0.4 s on one node) | design :750, :621 |
| D1: `lookup`/`stat`/`readdir` of a foreign object | 1 token RPC first touch or after a recall, 0 while cached; `readdir + stat` of a `C`-creator directory cold = `K` stripe tokens + `C` inode tokens (≈ 12 500 RPCs ≈ 20 ms at 64-way, 0 leaf reads), warm = RAM | design :752, :658 |
| D1: `write` to a FOREIGN-owned file | 1 custody grant + 1 publish ship per layout publish (3.6 verbs/MiB) + the recall fan-out | design :755 |
| D1: `unlink` of a foreign-minted file in MY directory | 1 ship per file + 1 barrier per plan | design :757 |
| D1: `rename` across two slots (file) / of a DIRECTORY | 1–3 ships + 1 barrier (+ 1–3 remote 4a guards) / + 1 set-wide lock RPC + `d` exact lookups | design :758–759 |
| D1: a broadcast write (1 writer, 12 500 readers) | 12 500 recalls per publish ≈ 100 ms at 64-way | design :655, :761 |
| Recall RTT, 64 in-process readers, one `setattr` | `send` 1 384 804 + `drain` 401 135 + `ack` 514 468 = `total` 2 300 407 ns (2.30 ms), fan-out p99 = 64 | `.benchmarks/…pr5` §3 |
| `free_grace_hold_ms` under tokens | ≈ 2.3 ms (vs the 2 724 ms epoch-fan-in composite) | same |
| Control-plane poll | idle 4 KiB; 2 checkpoints 12 KiB; torn slot 4 + 128 KiB | same |
| Create into a foreign directory | 863 µs/op (72 % the initiator's barrier, 10 % the ship); `plan` 38 · `intent_barrier` 625 · `ship_rtt` 83/step · `retire` 21 · `total` 837 µs | `.benchmarks/…pr6` §4 |
| Directory rename across holders | 2 659 µs/op; `dir_rename_lock_wait_ns` 537 µs uncontended | same |
| Creator wave 2 000 creates, 7 holders K = 14 / 31 holders K = 62 | `dir_stripe_ships` 2 000 = foreign creates; per-stripe max ÷ mean 1.09–1.19 / 1.46; ≈ 1.75 / 1.6 ms per create; **0 handovers** | `.benchmarks/…pr7b` §4 row 1 |
| Flip / migration / `readdir` / `lookup` (K = 14, 2 000 names) | flip 6.7–7.3 ms (10.8–12.4 ms with 7 suppliers); migration 908–960 µs/name; `readdir` striped 9.6 ms vs flat 14.6 ms; `lookup` striped 15.4 µs vs flat 22.6 µs | same, rows 2–3 |

---------------------------------------------------------------------------
## 6. Tests (name — one sentence; N-daemon needs)
---------------------------------------------------------------------------

Venue law: every contract runs IN-PROCESS over the two-holder declared-partition model with a real
loopback wire session (the reference's `sym_cross_owner_tests.rs:245 meta_router`, `sym_dir_stripe_
tests.rs:246`); the N-daemon rows (`sym_n_daemon_tests.rs`, 17 312 lines) need N processes on one
volume set and are the venue's `scripts/venue` fleet leg. Z/: the portable models (`expectHolds` /
`expectFails`) run everywhere; everything below is Linux-only under `builtin.os.tag == .linux`.

**Tokens** (`R/tests/sym_coherence_tests.rs`): `a_foreign_create_is_visible_at_the_readers_next_resolve`
:718 — exact, never bounded; `a_token_is_recalled_before_the_conflicting_commit_lands` :951;
`a_first_touch_grant_inside_the_pass_window_serves_the_committed_records` :1022 (the park);
`a_recall_storm_on_one_object_is_one_batch_per_pass` :1104 (16 co-queued commits, ONE batch);
`a_reader_never_acks_a_recall_with_a_read_in_flight` :1179 (the `ServeStamp` drain);
`a_grant_under_a_pending_recall_registers_afresh_and_stays_recallable` :1248; `a_reader_past_t_self_
serves_nothing_from_cache` :1334; `an_unacked_recall_completes_at_the_readers_lease_expiry` :1374 (64
objects, `expired_with_lease` +64, `timeouts_live` 0); `a_free_never_ships_before_every_recall_is_acked_
or_expired` :1530; `the_broadcast_shape_recalls_every_reader_once_per_publish` :1601 (fan-out p99 = R,
rtt exact-sum pinned); `a_voluntary_release_drains_and_purges_before_the_holder_is_told` :1705;
`a_single_flight_fetch_loser_registers_before_it_rechecks_the_winner` :1996; `the_token_cache_is_bounded_
by_bytes_and_refuses_an_oversize_entry` :2071; `a_recall_purges_the_recalled_objects_block_keys_and_
leaves_the_rest` :2143; `a_recall_never_drops_the_writers_dirty_layout_entry` :2270; `a_grant_on_a_slot_
another_appender_leases_is_answered_not_holder` :2409; `a_failed_windows_rollback_recalls_the_readers_
holding_its_records` :2489 (the gate's second user); `the_dotdot_reconnect_scan_fails_closed_on_a_token_
reader` :2589; `a_parked_grant_does_not_serialize_the_volumes_other_grants` :2642 (the session pool);
`a_displacing_publishs_recall_completes_before_the_holders_free_clears_the_bit` :3032; `a_cross_owner_
create_recalls_the_parents_token_before_its_shipped_step_applies` :3363; `the_grant_pass_gate_keeps_an_
object_in_flight_until_its_last_user_settles` :3584 (the refcount); `an_evicted_readers_grants_are_
swept_at_the_owners_eviction_not_at_the_deadline` :3647; `the_holder_grants_members_only_and_refuses_
a_ghost_before_it_registers` :3801; `a_grant_landing_inside_an_evictions_window_is_expired_not_unknown`
:3934; `a_recall_landing_inside_the_fetchs_eviction_never_installs_pre_commit_records` :4055;
`an_objects_xattrs_are_paged_under_the_grant_budget_and_never_resent` :4139; `a_single_writer_volume_
carries_no_token_plane` :4217; `a_ro_mount_under_the_knob_arms_the_token_client_and_writes_nothing`
:4333 (image byte-identical across the session); `a_readers_poll_scans_the_whole_ledger_after_a_ring_
of_stopped_polls` :4511 (the gap belt); `a_read_refused_not_a_member_parks_on_the_members_reclaim_and_
never_answers_eio` :4642; `a_readers_census_bounds_no_slot_the_writer_leases_and_follows_its_native_
watermark` :4860; `a_token_reader_lists_a_striped_directory_as_the_merge_of_its_stripes` :1784 and
`…holding_a_directorys_token_across_its_flip_lists_the_merge` :1882; `a_readers_root_attr_survives_the_
holders_connection_cap_so_stats_never_fail` :844.
**`-o ro` / quiescent** (`readonly_mount_tests.rs`): `a_token_readers_metadata_bound_is_zero_and_its_ttls_
derive_from_it` :1280; `the_mount_path_token_arm_refuses_loud_on_a_flat_volume` :1335 / `…without_a_
membership_lease` :1369; `a_read_only_open_of_a_quiescent_set_serves_the_projection_without_a_token_
client` :1403; `a_writer_appearing_under_a_quiescent_reader_fail_stops_it_within_the_poll` :1439;
`a_ledger_advance_under_a_quiescent_reader_is_a_writer_and_is_never_adopted` :1514; `the_poll_reads_the_
predicted_slot_first_and_falls_back_on_a_torn_one` :1587 (bytes asserted exactly); `an_explicit_kernel_
ttl_on_a_token_reader_is_refused_loud` :1739.
**Models** (loom → `explorer.zig`): the three `token_grant_models` (§1.5) with both weakenings; the
reader install-under-generation model; the `ServeStamp` Dekker model.

**Cross-owner intents** (`sym_cross_owner_tests.rs`): `a_create_in_a_foreign_directory_ships_one_insert_
dentry_and_mints_in_the_creators_rotor` :431; `unlink_rmdir_link_rename_and_exchange_across_two_holders_
leave_a_byte_exact_tree` :520 (≥ 9 intents minted ≡ retired; offline fsck clean); `a_rename_across_
three_holders_ships_a_step_to_each_foreign_side` :615; `every_initiator_crash_window_of_a_foreign_create_
rolls_forward_with_no_acked_loss` :663 (the seam after 0..=steps+1); `a_holder_dying_after_commit_before_
reply_is_recognized_exactly_once_by_its_successor` :730; `a_shipped_step_refused_slot_busy_at_its_holder_
is_redispatched` :801; `a_step_whose_slot_moved_to_the_initiator_mid_plan_applies_under_its_own_guard`
:872; `a_holder_dying_before_commit_leaves_an_open_intent_the_cadence_rolls_forward` :967; `all_parties_
down_after_a_foreign_unlink_the_next_mount_rolls_forward` :1035; **`two_nodes_cannot_rename_directories_
into_a_cycle`** :1086 (two identities, disjoint guard sets, the loser WAITS ≥ 200 ms on `Busy` then
`EINVAL`; with the lease a no-op it FAILS at "the loser waited"); `a_file_rename_never_takes_the_dir_
rename_lock_and_every_directory_rename_does` :1253; `the_dir_rename_lock_is_durable_in_tree0_and_a_dead_
holders_lock_is_released_by_recovery` :1332 (a wire joiner); `two_directory_renames_of_one_identity_
serialize_under_the_lease` :1400; `the_lock_verbs_reject_a_foreign_unlock_a_dead_id_and_any_volume_but_
zero` :1489; `the_flat_paths_ship_nothing_and_lock_nothing` :1575; `the_cross_owner_family_is_exported_
under_its_published_names` :1645; `a_live_op_spanning_cadence_passes_is_never_replayed_by_the_cadence`
:1695; `a_retired_intent_met_by_a_stale_scan_is_a_no_op_and_leaves_no_ghost` :1769; `a_served_step_never_
parks_behind_the_initiators_guards_even_on_a_stripe_collision` :1848 (the §2.4 finding, red at
`SQUEEZEFS_DLM_STRIPES=1`); `a_directory_rename_whose_ancestor_link_collides_with_its_own_guard_stripe_
completes` :1902; `an_intent_homed_in_a_slot_another_appender_leases_is_never_adopted_here` :1957;
`a_remote_initiators_guards_park_at_the_holder_until_release_and_expire_at_the_grace_belt` :2028;
`a_parked_scope_expires_with_its_initiators_membership_lease_when_a_plane_is_armed` :2173; `an_initiator_
acquires_its_foreign_guards_at_the_holder_and_releases_them_at_the_end` :2287; `every_cross_owner_verb_
pays_one_guard_round_trip_per_foreign_holder_table` :2348 (create/unlink/link/rename 1; a two-holder
rename 2); `a_parked_xv_guards_never_blocks_the_release_that_unparks_it` :2430; `a_step_outside_its_
scopes_parked_keys_takes_its_own_guards_and_parks` :2504; `a_local_step_outside_its_scopes_keys_takes_
its_own_guards_never_unguarded` :2589; `a_served_insert_naming_an_unmintable_child_is_refused_and_
plants_nothing` :2664; `a_live_refusal_compensates_a_links_count_and_a_renames_removed_source` :2717;
the `#[ignore]`d `scoping_row_per_verb_wire_and_barrier_cost` :2787. `crossvol_tx_tests.rs`: the seam
matrix `crossvol_{unlink,link,dir_rename}_seam_recovers_pre_or_post_never_intermediate` :216/:308/:349,
`recovery_is_idempotent_across_repeated_mounts` :616, `recovery_skips_a_foreign_occupant_instead_of_
clobbering` :666, `concurrent_inverse_crossvol_ops_never_deadlock` :766, `intent_record_round_trips_and_
never_panics_on_hostile_bytes` :818 (→ `std.testing.fuzz`).
**N-daemon** (`sym_n_daemon_tests.rs`): `a_joiner_reads_foreign_slots_through_tokens_and_serves_the_
managers_shipped_step` :4870; `a_dead_joiners_half_applied_cross_owner_unlink_is_rolled_forward_at_the_
manager` :5070; `a_joiners_setattr_of_the_managers_file_lands_at_the_holder` :5320 and the reverse
:5940; `a_served_chmod_never_drops_the_holders_dirty_layout_entry` :5730; `a_foreign_slot_verb_whose_
holder_is_unreachable_refuses_retryable_then_lands_once_bound` :6094; `a_roll_forwards_local_step_
refused_slot_busy_past_the_bound_leaves_the_intent_open` :6289; `a_peers_lookup_through_a_dead_holder_
is_refused_inside_the_bound_and_exact_after_recovery` :6582; `a_live_joiner_follows_a_manager_failover_
to_the_successors_listener` :6963 (§1.10); `a_stale_holder_view_at_the_guards_re_resolves_and_lands_the_
create` :2867; `a_locally_dispatched_create_whose_slot_the_manager_took_is_redispatched_through_the_
cross_owner_arm` :2974; `a_local_steps_slot_busy_mid_plan_redispatches_and_never_fail_stops_the_
initiator` :3084; `a_joiners_first_shipped_step_after_a_failover_retries_the_stale_era_once` :586.

**Record ship / kernel** (`served_mutation_kernel_tests.rs`): `a_served_data_mutation_invalidates_the_
whole_inode_then_prunes_it` :54; `a_served_attrs_mutation_invalidates_attrs_only_then_prunes` :76;
`without_prune_capability_only_the_invalidation_travels` :95; `the_hook_delivers_from_a_plain_os_thread_
without_a_runtime` :112; `an_unarmed_flat_writer_never_reaches_the_served_mutation_sink` :138.

**Striping** (`sym_dir_stripe_tests.rs`): `names_route_to_hash_mod_k_and_are_served_from_exactly_one_
place_during_migration` :641; `a_flip_is_idempotent_and_its_k_is_clamped` :767; `readdir_merges_k_stripes_
in_hash_order_with_a_stable_continuation` :809; `a_crashed_flip_completes_on_the_successor_lessee` :884;
`a_name_in_both_homes_during_migration_is_served_from_the_stripe` :952; `rmdir_of_a_striped_directory_
never_races_a_create` :1049 (the R26 pin); `a_killed_striped_rmdir_rolls_forward_whole_at_every_window`
:1162; `rmdir_of_a_migrating_directory_with_an_unmigrated_name_answers_enotempty` :1243; `a_striped_
directorys_nlink_and_times_are_exact` :1333; `rename_link_and_unlink_route_through_the_stripes` :1397;
`a_directory_flips_at_the_derived_trigger_and_never_below_it` :1466; `a_stripe_never_flips_itself` :1554;
`dotdot_of_a_striped_directory_resolves_to_its_parent_whatever_the_key_order` :1656; `mutations_racing_
the_migration_never_answer_enoent_or_strand_a_name` :1720; `a_preset_create_into_a_striped_parent_is_
refused` :1845; `a_creator_wave_spreads_over_the_stripe_holders` :2002 (N = 7/31, 0 handovers);
`stripes_supplied_by_other_appenders_are_served_by_their_own_holders` :2126; `the_knob_at_one_never_
stripes_and_still_routes_an_existing_map` :2314; `stripe_dirs_at_mkdir_stripes_every_new_directory` :2371;
`the_flat_paths_stripe_nothing` :2533; `every_planted_c17_shape_is_found_and_nothing_else` :2647;
`a_flip_takes_its_4a_guards_in_one_acquisition_under_the_width_one_crucible` (the Issue 28 pin at
`SQUEEZEFS_DLM_STRIPES=1`); N-daemon: `a_foreign_create_into_a_striped_directory_reads_the_stripes_
record_at_its_holder` :2001, `a_joiners_stat_of_a_striped_directory_folds_the_stripes_at_their_holder`
:2179, `a_striped_directorys_stat_at_the_holder_survives_a_suppliers_death` :2270, `ships_into_a_
striped_directory_feed_no_dominance_window` :2767.

---------------------------------------------------------------------------
## 7. QUIRKS (decision 0002) and the better designs
---------------------------------------------------------------------------

Each becomes a row in 0002's ledger before the wave diverges.

| Q | Reference quirk | Why it is poor | Z/ |
|---|---|---|---|
| Q1 | Every token frame, every `MetaCall`/`MetaReply` and every `XvStep` on the wire is `bincode` + serde (`token_plane.rs:398–422`; `wire.rs:341`); client ids are `String`s hashed on every holder table op (`token_grant_core.rs:66`, `TOKEN_CLIENTS: scc::HashSet<String>` :643) | Row 2/53's class — a serialization-library artifact as the wire; string identities on a control path | §1.2's `extern struct` frames with length-framed bodies, xxh3 over the frame, total bounded decode (fuzzed); `client_id: u64` |
| Q2 | TWO foreign-read planes coexist: the S10 LOOKUP delegation plane (`dlm_delegation`, `deleg_read_gate`, `deleg_mutation_gate`, the `RecallLane`'s thrash VALVE/demotion/cooldown with `RECALL_THRASH_CYCLES = 3`, `RECALL_COOLDOWN_WINDOWS = 8`, `GrantDecision::Demoted`, `tokens.rs:725–1200`) and the PR 5 token plane that runs the lane with `valve: false` (:1095–1098, :1168–1179); the S8 fencing-token cache (`token_cache_core.rs`, `SQUEEZEFS_DLM_TOKEN_CACHE_MAX`) beside them | Two mechanisms for one law (R-SYM-4: ONE method); a valve that can never engage; three caches | ONE token plane; the `RecallLane` ported as the holder table + per-client frame issuer ALONE (no valve, no demotion, no cooldown, no `dlm_revoke_phase_ns`); the S8 fencing-token cache is not built (the custody plane carries its own epoch) |
| Q3 | The recall deadline derivation keeps a `4 × p99 + 200 ms` arm and three levers (`SQUEEZEFS_DLM_RECALL_{BATCH_MAX,DEADLINE_MS,COOLDOWN_MS}`) that the token lane never uses (`tokens.rs:758–762, 849–864`) | Row 17's class — a constant with knobs on top; dead arms | `recall_deadline = T_owner` (one `pub const` with its reason); `batch_max` derived from the frame cap alone; no levers |
| Q4 | Test seams as process-global `AtomicU64/AtomicBool` statics (`TEST_XV_*` ×14, `crossvol_tx.rs:167–288`; `TEST_DISPATCH_HOLD_AFTER_CHECK_MS`, `TEST_INSTALL_CARRIED_FAIL_ONCE`, `TEST_CUSTODY_NOT_HOLDER_ONCE`, `token_plane.rs:495–516`; `TEST_STRIPE_*`, `dir_stripe.rs:258–266`; `TEST_CONVEYOR_HOLD_STAGE`) | Row 85/157's shape — test posture through process globals; two fixtures in one process share them | `SQUEEZEFS_TEST_*` registry rows read once into a per-set `Seams` struct the tests construct directly |
| Q5 | Process-global registries: `OPEN_INTENTS`, `PARKED_GUARDS`, `LOCAL_SCOPES`, `XV_SHIPPER` (`crossvol_tx.rs:469, 868, 883, 801`), `TOKEN_CLIENTS`, `TOKEN_HOLDER_PLANES`, `TOKEN_READER_PLANES`, `TOKEN_RECORDS_BYTES`, the `RECALL_PURGE_*` statics (`token_plane.rs:643, 665, 2021–2026, 3691–3707`), `DIR_RENAME_LOCAL_PERMITS` (`crossvol_arms.rs:37–41`), `SERVED_MUTATION_SINK` (`record_ship.rs:502`) | Row 102's class — statics shared across mounts in one process; `HashMap::insert` under a mutex on an op path (row 32) | Every registry is a field of the `MetaSet` (one per mount), sized once from the membership census / the plan bound, `putAssumeCapacity`; the sinks are vtable fields on the set |
| Q6 | The live-refusal compensation's crash window: a kill between an applied step and the last inverse leaves the FORWARD plan to the roll-forward, which re-meets the refusal and leaves the halves for C9/C10 (`crossvol_tx.rs:3104–3111` — "the abort marker on the record is the recovery-side answer this rung does not build") | A stated census-class residue the record could close | `IntentHeaderWire.flags` bit 0 `ABORTING`: the compensation first flips the bit (one entry, barriered), then inverts; a roll-forward that reads the bit runs the INVERSE plan and retires |
| Q7 | `tx_id`'s 40-bit sequence seeded from `SystemTime::now()` and `DirRenameRecord.since_ns` from the wall clock (`crossvol_tx.rs:2070–2090`; `crossvol_arms.rs:354–356`) | Row 144's class — a wall clock steps; the term already makes ids unique | `tx_id = writer_term << 40 \| seq` with `seq` from a per-mount monotone counter recovered as `max(open intents' seq) + 1` at open; `since_ns` is `clock.realtimeCoarseS` (operator-facing, documented as such) |
| Q8 | The ancestry walk sleeps `checkpoint_landing_ceiling` between re-scans (`mod.rs:2430–2432`); the drain parks on a 10 ms `timeout` tick (`ro_coherence.rs:520`); `await_object_settled`/`await_recall_retired` on `sqz_time::timeout` (`token_plane.rs:943, 962`) | Sleep as synchronization (AGENTS.md); timer-dependent progress | Register-then-check on `sync.Notify` with `clock.monotonicNs` deadlines (the port's `tick` backstop counts rescues); the ancestry re-read waits on the parent memo's change notify |
| Q9 | The served context is a task-local `SHIP_CLIENT` (`meta_ship/service.rs:123–201`) consulted by `executing_for_ship_client()` at 6 sites (`record_ship.rs:300, 326, 403`; `crossvol_tx.rs:1125`) | An implicit parameter deciding lock acquisition and routing | An explicit `Served{client_id, scope}` argument on every applier/resolver entry |
| Q10 | `Recall { wait_ms }` is a LONG-POLL on a dial-only wire (`token_plane.rs:163–167, 1097–1127`): one standing session per reader per holder parked at the holder, `poll_park_bound` ≤ 1 s | A park per (reader, holder) at the holder; a 100 ms floor on recall delivery latency | The cluster wire is bidirectional in Z/ (one session per peer pair): the holder PUSHES `RecallFrameWire` on the session; the "fresh channel" law becomes a heartbeat on the same session; `dlm_token_channel_rounds` counts heartbeats |
| Q11 | `SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP` / `_OBSERVED` levers whose off legs are refused on a token reader and leave a skipped clean entry to a 1 s TTL on a writer (`ro_coherence.rs:525–569`, `token_plane.rs:3874–3882`) | Measured-loser/refused control arms kept as knobs (row 155's class) | Neither exists: the step gate and the observed drain are the law |
| Q12 | `channel_not_fresh` wears the WIRE class (`Io(NotConnected)` → EIO) so the root-attr projection arm can tell it from the law words (`token_plane.rs:2223–2235`) | An error class chosen for a downstream match | ONE typed `error.TokenChannelStale` (retryable) in `core/errors.zig`; the root-attr exception keys on it by name |
| Q13 | The token-oversize refusal ("a directory too large for this reader's budget — the streaming form is owed", `token_plane.rs:2123–2125, 2941–2953`) | A directory larger than `mem_budget/256` cannot be listed by a reader at all | The reader's dentry set is PAGED-RESIDENT: the entry holds `[cookie → page]` windows under the budget (the `kvmap.WindowCache` shape, 0002 row 194); a recall drops the whole set |
| Q14 | Striping's "stripe-at-flip" supplies ask creators one `SupplyStripeIno` RPC each for `K` stripes (`dir_stripe.rs:1200–1215`); the explicit flip from a non-holder answers EAGAIN "naming the holder today" (design :622 item 7) | An unshipped command; `K` serial RPCs | `SupplyStripeInos{dir, indices[]}` batched per creator; the explicit flip ships as ONE `MetaCall::StripeDir{dir, k}` to the holder |
| Q15 | `IsEmpty` of a REMOTE stripe under the served context answers under the holder's own guard — a file create can land between the probe and the dying mark (design :622 item 9, "unreachable today") | A stated hole in the one-intent rmdir | §4.7: the served `IsEmpty` takes the stripe's EXCLUSIVE `I{D_i}` under the travelling scope's park, held to the intent's terminal outcome |
| Q16 | `kernel_prune_capable()` reads `fuse3::raw::negotiated_reply_flags()` / `kernel_init_info()` process globals and announces the degrade with a `Once` (`fuse_client.rs:31395–31419`) | Session facts read through crate globals | `session.InitCaps{writeback_cache, prune_capable}` passed to the sink at install |
| Q17 | `record_object_ino` reads the ino by RAW offset per kind with a `forest` flag selecting 16 vs 17 for refs (`backend.rs:19484–19494`) | Row 35's class — a kind-dependent framing flag | v4 has ONE key layout: `key.slotTreeKeyIno(kind, key)` (the kind byte at offset 8 for ino-major kinds; refs by owner at the fixed v4 offset) |
| Q18 | The `dir_rename` lease record is a hand-laid `version u8 ‖ LE` 21 B value (`slot_state.rs:965–1020`) | Fine in substance; not an `extern struct` | `DirRenameRecordWire` 24 B (§2.10) |
| Q20 | `is_stripe` is a REVERSE SCAN to the candidate's parent through `..` (`dir_stripe.rs:1002–1007`); a stripe's record is a plain nameless directory | A nameless stripe has no parent memo at a holder that acquired its slot by handover or recovery; every served insert into a stripe would pay the walk | `record.INODE_FLAG_STRIPE` on the stripe's record (0002 row 493; 0003 §Q.1): the flip and the served census read it |
| Q21 | Phase A's `SupplyStripeIno` is one RPC per stripe, one stripe per creator; the explicit flip from a non-holder is unshipped (EAGAIN) | K serial RPCs; an unshipped command | `SupplyStripeInos{dir, indices[]}` ONE batch per creator at the share `K / (creators + 1)` (0x0306); the explicit flip ships as the `setxattr` COMMAND over `record_ship` and the holder's service runs it (0002 row 494; 0003 §Q.2–Q.3) |
| Q22 | The shipped layout publish carries the record alone (`publish.rs:2200–2253`); the served Put stages the xattr — the owner-tree refs diff and the DMA-proof law have no served arm | A layout without its refs is C8 drift; a publish without proofs launders the type-flip law | `RecordShipHead.n_proofs` + `DmaProofWire[]`; the holder's `PublishFace` runs `publishLayout` with the diff and the proofs re-minted from the wire (0002 row 495; 0003 §Q.5) |
| Q23 | The handover release keeps the offer as a memo any grant spends, "whoever lands it" (`slot_lease_core.rs:906–935`, :718–721) | The requester's own ships, still served at the releasing holder, first-touch the slot back in the release → grant gap; the accept is refused | The stand is a reservation: other first touches refused naming the accepter, the rotor skips it (0002 row 502; 0003 §Q.11) |
| Q24 | `xv_serve_guards` awaits ONE `lock_many` at the holder (`meta_backend/mod.rs:2133–2200`) — an async task per request | Over a per-connection sequential serve with every call of a daemon multiplexed on one session, a parked serve is head-of-line for the very scope that holds the stripe | TRY form + `Busy`; the initiator retries with a doubling 1 → 16 ms backoff under the reply bound (0002 row 503; 0003 §Q.14) |
| Q25 | `lease_verdict` reads the INSTALLED owner (`token_plane.rs:851–874`), the manager's process only: a joiner holder judges every reader `Unknown` | A dead reader's recall waits the 45 s deadline at a joiner holder; its parked scopes hold their stripes the whole belt | The joiner's census view (the owner's census per renew interval, absent ⇒ expired only against an older contact), the census commit sweeping the holder at once (0002 row 504; 0003 §Q.15) |
| Q26 | The roll-forward applies the forward plan and SKIPS a step its witness refuses before retiring — "re-meeting the refusal — the C9/C10 census classes" (`crossvol_tx.rs:3104–3111`) | Every ship failed against a dead holder leaves an open intent and an EAGAIN; the caller's retry of the same name wins it, and the skip orphans the first attempt's minted child | The roll-forward compensates on a re-met refusal: the inverses of the steps applied by the pass and of the fresh mints found applied, then the retirement (0002 row 505; 0003 §Q.19) |
| Q27 | `stripe_locate` reads the stripe, then `D`'s own tree, and answers `None` on a miss in both (`dir_stripe.rs:810–830`) | A move out of `D` (insert into the stripe, then remove from `D`) between the two reads makes an existing name ENOENT | A miss in both reads the stripe again (0002 row 506; 0003 §Q.20) |
| Q19 | `atomicity.rs` reads sysfs with `std::fs::read_to_string` (deliberately off `uring_fs`, :14–19) and is unrelated to this surface | — (listed because the brief named it) | Not this spec's; the sector-atomicity probe is `io/align_probe.zig`'s business already |

---------------------------------------------------------------------------
## 8. Z/ landing shape (what each module owes)
---------------------------------------------------------------------------

- `src/meta/token/{wire,holder,reader,gate,sink}.zig` — §1: `TokenHeadWire` + bodies (fuzzed); `Holder
  {lane: HolderTable, pending_frames, gate: GrantPassGate, stats}` with `recallAndWait(io, union) ![]u64`
  / `settle(union)` / `serveGrant` / `recallSlot`; `Reader{cache, fetching: sync.flight, revoke_gens,
  channel, budget}` with `serve(io, object, wants) !?Serve`; `token_gate_model.zig`, `token_install_
  model.zig`, `serve_stamp_model.zig` under `model/explorer.zig` with their weakenings.
- `src/meta/conveyor.zig` — A1½ (§1.4): `deps.tokens.?.recallAndWait(union)` before `admitUserBudget`;
  `settle` in `Window.finish` for both arms; `pass_token_recall` in `pass_total`'s decomposition.
- `src/meta/revalidate.zig` — §1.11 the predicted-slot poll + the gap belt + the quiescent verdict
  over `epoch_core.ReaderGate`.
- `src/data/read_path.zig` / `plane.zig` — the per-thread `ServeStamp` (§1.7) at the read serve's start;
  `MountRecallSink.drainAndPurge` through `purge.Tiers`; `Epochs`'s dirty authority for `DirtyKept`.
- `src/meta/intent.zig` — §2.1–2.3, §2.7–2.8: the wire, `Plan`, `execute`, `recoverOpen`, `rollForward`,
  the register + cadence (a `sqz-meta` fiber on the set's lane at the landing ceiling), the stats.
- `src/meta/guards.zig` — §2.4: `acquireLeased(io, set, scope, inos, dents, discovery)` over
  `dlm.Manager.lockMany` + `XvGuards`/`XvRelease`; `ParkedScopes`; `serveGuardsFor`; `GuardSet` with
  the external guard and the `2 × 64 + 8` plan bound.
- `src/meta/meta_set.zig` — delete `commitSplit`/`CrossVolume`/the three `routing_split*` rows; add
  `tokenServe`, `readInodeWitness`, `shipRecordVerb`, `serveXvStep`, `lookupExact`, `dirRenameLease`.
- `src/fuse/handler/kv_fs.zig` — the plan builders at the five `t.cross` sites (§2.5); the striped
  prelude on every dentry op (§4.5); the served-mutation + recalled-object sinks (§3.4) over
  `session.notifyInvalInode`/`notifyPrune`; `user.squeezefs.stripes` as a command before the allowlist.
- `src/meta/dir_stripe.zig` — §4 whole; `src/meta/fsck/c17.zig` the report-only census.
- `stats/stats.zig` — the three families as generated namespaces (0002 row 197); `tests/rails.zig`
  stays green only if every row above is READ by a module.
