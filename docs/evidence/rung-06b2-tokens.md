# Rung 6 wave B2 — GPFS-strict read tokens (evidence)

Charter §5.5 "Tokens — GPFS-strict, the ONLY foreign-read method"; spec
`docs/porting/sym_tokens_intents_striping.md` §1, §5–§8; decision 0002 rows 257–262.

## What landed

- `src/meta/sym/tokens/token_table.zig` — the holder: per-object grants (object = inode +
  carried xattrs; a directory's dentry set paged by cookie), the bounded table (objects ×
  clients, `putAssumeCapacity`), `GrantPassGate` (a REFCOUNT under one lock; `passBegin` marks
  THEN the lane reads holders; `serveGrant` registers THEN checks — a grant on an object in
  flight PARKS to the settle, bounded by the deadline), `recallAndWait` (A1½: sweep dead
  readers, recall every holder ONCE, ONE frame per reader, wait on every ack or the lease
  expiry; `dlm_token_recall_timeouts_live` must-stay-0 for a LIVE member past the TTL),
  `serveAck/serveRelease/noteMemberDeparted`, the page builder under `CONTROL_CAP / 2`.
- `src/meta/sym/tokens/token_cache.zig` — the reader: immutable pinned entries, install under
  `gen0` decided under the cache lock (bump BEFORE remove), single-flight per object, the
  records budget `max(mem_budget/256, 1 MiB)` as the R5 component `dlm_token_records_bytes`
  (floor 0, weight 1, shed = drop without release), voluntary eviction (second-chance `used`),
  `ServeStamps` (per-thread shards × 64 generation slots, the seq_cst Dekker re-read, the
  observed drain on a Notify with the 10 ms backstop, the poison mask), the pushed recall's
  ack conditions (step → drain → scoped purge; DIRTY kept, held skipped, census fallback once),
  the deferred-ack drain lane, the serve gate (`T_self`, `channel_ok`), the re-point.
- `src/meta/sym/tokens/read_divert.zig` — `Divert.serve` (local ⇒ null; foreign ⇒ the plane,
  bound ONCE off the resolver; a `NotHolder` followed once; a dead lessee never dialled),
  `HolderService` (Grant/RecallAck/Release/CustodyGrant under the frame law), `ReaderService`
  (the pushed Recall), `LinkPusher`, `KernelInvalidator` (INVAL_INODE + PRUNE on the global ino).
- `src/meta/sym/tokens/ro_mount.zig` — reader TTLs derive to 0 (an explicit `-o *_timeout`
  refuses loud), `MemberLease`/`OwnerLeases` (`T_owner`), the quiescent reader's fail-stop
  (`ReaderFailStopped` → ESTALE, root attrs excepted), `EpochPoll` (predicted slot, successor
  walk, torn → whole ledger, 32-poll gap belt).
- Hooks: `conveyor.TokenHook` armed on the pass (A1½ between the liveness flags and ring
  admission; `pass_token_recall` in the phase decomposition; the settle in `finishWindow` for
  both arms); `kv_fs` reads (`readInodeOpt`, `findDentry`, `listDir`, `xattrValue`,
  `listxattr`) through `tokenServe`; `Session.tokenNotifySeam`; `MountOptions.explicit_ttls`.
- Wire: `token_recall` is a PUSH (`RecallFrame` → `RecallVerdict`), row 257.

## Models (`model/explorer.zig`, every weakening proven to fail)

| Model | Holds | Weakening fails |
|---|---|---|
| first-touch grant racing the pass | recalled ∨ parked | the grant's two steps swapped |
| two readers racing the pass | recalled + parked ≥ decided-in-window | the same swap |
| two overlapping users | a grant inside user 2's window parks | refcount → set |
| ServeStamp drain (TSO) | no pre-step serve escapes the drain | the serve's fence (re-read) dropped |
| install under generation | pre-commit pages never installed after the removal | remove-then-bump |

## Contracts (in-process, loopback hub, real wire sessions)

grant → cached (0 RPCs) → commit recalls → inline ack → fresh re-grant; the directory token's
cookie pages; a recall during an in-flight serve is DEFERRED and acked by the drain lane after
the serve ends; DIRTY kept; a first-touch grant inside the pass window parks (the seam's hold)
and reads the committed records; an unacked recall completes at the lease expiry (timeouts_live
0); a LIVE member past the deadline fires the tripwire once; the broadcast shape (8 readers, ONE
batch for 16 co-queued txs, exact-sum `send + drain + ack ≡ total`); `CustodyGrant` carries the
epoch + the page, a directory is rejected; `NotHolder` redirect followed once; the serve gate
and the re-point; the conveyor's A1½ contract on the RAM lane (both arms settle). Linux: the
smoke stage over one real volume — two holder planes on the real pass fiber, one `-o ro` token
reader over the wire, the epoch poll's predicted slot on the device after `checkpointNow`.

## Owed

The paged-resident dentry set (spec Q13: an entry over the whole budget is still refused loud);
the in-place join of a fail-stopped quiescent reader; the Linux smoke's second writer is a
second HOLDER PLANE over one backend (a second in-process REGION's writes need the slot plane's
door, wave B1); the divert in `kv_fs` is bound by the daemon's mount wiring once the slot plane
publishes endpoints (`SlotHolderCache` → `read_divert.Resolver`).
