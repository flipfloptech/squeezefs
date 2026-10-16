# PORTING SPEC — `guard_claims` (D0 ladder layers B1 + B2, the clients/claims/heartbeat plane)

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

Charter: §5.7 (the three layers, the ordering law, the recovery ladder), §5.5 "Fencing"
(the shared PR primitive; WERO is rung 6), §8 (`clients`, `claim clear`, the register
ladder on nvmet, "nvmet has no PTPL"), §1.8 (the fail-stop lattice). Rung 5 of §13.

Reference files (all paths under `/Users/joberdorf/Source/squeezefs`; `B:` =
`src/meta_backend/kv/backend.rs` (28,730 lines), `R:` = `src/meta_backend/reservation.rs`
(1,576), `M:` = `src/membership.rs` (4,150), `MW:` = `src/membership_wire.rs` (957),
`MI:` = `src/member_id.rs` (50), `LC:` = `src/lease_clock_core.rs` (344), `FC:` =
`src/fuse_client.rs`, `MAIN:` = `src/main.rs`, `INIT:` = `src/nvmeof/initiator.rs`,
`DC:` = `src/data_custody.rs` (1,435), `DMT:` = `docs/design-metadata-throughput.md`,
`T:` = `tests/mount_writer_guard_tests.rs` (1,824), `TM:` = `tests/dlm_membership_tests.rs`
(2,748), `TS:` = `tests/sym_fence_tests.rs`, `TW:` = `tests/wero_rtype_tests.rs`,
`TC:` = `tests/cli_clients_df_tests.rs`, `LOOM:` = `loom-models/src/lib.rs`).

Port files touched (read-only for this spec): `src/meta/backend.zig` (layer A at L646-672,
the open ladder L310-591, `close` L1059), `src/meta/journal.zig` `FailureLatch` L1064-1111,
`src/meta/journal_lane.zig` `runVerdicts` L1225-1271 / `barrierDone` L1352-1383,
`src/meta/checkpoint_cycle.zig` `syncDevice` L543-554, `src/io/nvme_derive.zig`
`isReservationConflict` L250 / `Fence` L470-487 / `poisonCustody` L452, `src/io/nvme_dev.zig`
`classifyDmaOutcome` L1239-1245, `src/io/uring128.zig` `prepUringCmd` L617-623,
`src/core/errors.zig` (`.BADE ↔ error.ReservationConflict` L159; `Busy ↔ EBUSY` L160).

## Rung map (what this spec ports now, what it marks for later)

| Mechanism | Rung | Where below |
|---|---|---|
| Layer A `flock` on a dedicated fd | **landed** (`backend.zig` L646-672) — rung 5 adds the holder-naming refusal + transient-holder absorption | §2.6 |
| B1: RESCAP probe, register ladder, acquire WE (rtype 1), preempt, release, report, heartbeat re-check, fence latch | **5** (the primitive is shared with rung 6) | §1 |
| B2: `writer_claim` record, gate decision, ordering law, heartbeat, `claim clear` | **5** | §2 |
| `client:{id}` registrations, `clients` verb, `format` preflight live-sweep | **5** | §3.1-3.3 |
| `writer_term` record (DLM S2 era ladder, incompat bit 7) | **6** (tokens/custody epochs); the claim record RESERVES the field now | §2.1 |
| WERO rtype 3, registrants, `preempt_and_abort`, registrant-cap gate, `pr_registrant_shared` | **6** (manager/appenders); the client methods ship with the primitive, inert on a flat volume | §1.2, §1.7 |
| `membership_owner` / `claim_set` records, the JOIN/RENEW/LEAVE/CENSUS wire, `T_self` lease clocks, grace window, eviction → death ledger | **6** (`SQUEEZEFS_MEMBERSHIP_BIND` is `off` by default — M:2820, 3049-3055) | §3.4 |
| `lease_clock_core` + its loom model | **6**, but the pure arithmetic + the Zig model are cheap and portable: write them in rung 5 so rung 6 inherits a proven core | §3.5 |
| Real-device legs (nvmet-loop RESCAP, EDS fallback, SC 0x18, PTPL lapse) | **8** (nvmet target management) | §5.3 |

---

## 1. Layer B1 — the NVMe Persistent Reservation primitive

### 1.1 Posture decision (`resolve_for_mount`, R:761-784)

1. A test override installed for the path wins (R:715-739; the port's seam is an injected
   `ReservationClient` vtable — §5.2).
2. Else open the real client: `NvmeReservationClient::open` (R:867-890) requires a BLOCK
   device that answers `NVME_IOCTL_ID` (`_IO('N',0x40)` = `0x4E40`, R:816) with nsid > 0;
   regular files, loop devices and non-NVMe nodes → `None` = **no PR, detection grade**.
3. `rescap()` = Identify Namespace CNS 0, byte 31 (R:1026-1038). `0` → `None` with the log
   "namespace advertises no reservation support (RESCAP=0) — single-writer guard degrades to
   detection grade (flock+claim)" (R:769-774); a probe error → `None` at debug level
   (R:776-782). **Never fails the mount** (R:757-759).

**Plain-file substrate skips PR by construction** (step 2); `single_kernel_substrate`
(R:751-753) = regular file ∧ no override — one kernel, the flock IS the fence; this is what
lets the symmetric arm admit a file volume without the non-PR opt-in (B:18462-18470, rung 6).

**Port note — the char device.** Linux exposes NVMe passthru through `IORING_OP_URING_CMD` only
on the generic CHAR node `/dev/ngXnY` (`nvme_ns_chr_uring_cmd`); the block node's uring_cmd is
the block layer's own (`BLOCK_URING_CMD_DISCARD`, which the port already uses —
`nvme_dev.zig` L880-885, L956). The port therefore: (a) classifies the meta path as an NVMe
namespace by reading `/sys/class/block/<name>/nsid` (present only on NVMe namespaces; its
value IS the nsid — no `NVME_IOCTL_ID` ioctl); (b) derives the char twin by name
(`nvmeXnY → ngXnY`, same instance numbers by kernel construction) and verifies it through
`/sys/class/nvme-generic/ngXnY/dev` (major:minor); (c) opens `/dev/ngXnY` `O_RDWR|O_CLOEXEC`
as a dedicated guard-lifetime fd. A namespace with no `ng` twin refuses loud (a kernel
without `CONFIG_NVME_... generic` nodes is a stock-kernel degradation, §1.4 — log it, degrade
to detection grade, count `writer_guard_pr_unavailable`).

### 1.2 Every NVMe command used (R:823-828, 830-846, 925-1022, 1026-1089)

All issued as one-shot passthru. Reference: `ioctl(NVME_IOCTL_IO_CMD = _IOWR('N',0x43) =
0xC048_4E43)` / `NVME_IOCTL_ADMIN_CMD = 0xC048_4E41` on the block fd (R:816-820) with
`struct nvme_passthru_cmd` (R:792-813, 72 B + result). **Port:** `IORING_OP_URING_CMD` on
the char fd via `uring128.prepUringCmd(sqe, file, cmd_op, cmd)` with `cmd_op =
NVME_URING_CMD_IO = _IOWR('N',0x80, nvme_uring_cmd) = 0xC048_4E80` for NVM commands and
`NVME_URING_CMD_ADMIN = _IOWR('N',0x82) = 0xC048_4E82` for admin commands (uapi
`linux/nvme_ioctl.h` L109-112 on the venue kernel). The 72 B `struct nvme_uring_cmd`
(`opcode u8, flags u8, rsvd1 u16, nsid u32, cdw2 u32, cdw3 u32, metadata u64, addr u64,
metadata_len u32, data_len u32, cdw10..cdw15 u32×6, timeout_ms u32, rsvd2 u32` — uapi L74-93)
goes at the head of the 80 B SQE128 `cmd` area, zero-padded; the ring must be
`SQE128|CQE32` (`uring128.Ring128.init` L176-210 already sets both). CQE: `res` = NVMe
status (`SCT‖SC`, positive) or `-errno` (transport), `res2` = CDW0/CDW1 result
(`nvme_uring_task_cb` → `io_uring_cmd_done(status, result)`). Reservation Conflict is
status `0x83` (R:846; match `res & 0x7ff == 0x83`, R:915); the block-layer form `-EBADE`
(R:905-909) cannot appear on passthru but keep the OR for one classifier.

| Command | Opcode | nsid | CDW10 | CDW11 | Data | Lines |
|---|---|---|---|---|---|---|
| Identify Namespace (RESCAP) | admin `0x06` | ns | `0` (CNS 0) | 0 | 4096 B in; RESCAP = byte 31 | R:1026-1038 |
| Get Features Host Identifier (FID 0x81) | admin `0x0a` | **0** | `0x81` (SEL 0 current) | bit 0 `EXHID` | 16 B in (EXHID=1); fallback 8 B (EXHID=0) for PCIe 64-bit hostid; all-zero ⇒ "unknown" (empty) | R:1056-1089 |
| Reservation Register | NVM `0x0d` | ns | bits 2:0 `RREGA` (0 register, 1 unregister, 2 replace), bit 3 `IEKEY`, bits 31:30 `CPTPL` | 0 | 16 B out `crkey u64 LE ‖ nrkey u64 LE` | R:927-940 |
| Reservation Report | NVM `0x0e` | ns | `NUMD` = `len/4 − 1` (0-based dwords) | bit 0 `EDS` | `len` B in | R:944-958 |
| Reservation Acquire | NVM `0x11` | ns | bits 2:0 `RACQA` (0 acquire, 1 preempt, 2 preempt-and-abort), bits 15:8 `RTYPE` | 0 | 16 B out `crkey ‖ prkey` | R:987-1000 |
| Reservation Release | NVM `0x15` | ns | bits 2:0 `RRELA` = 0 (release), bits 15:8 `RTYPE` | 0 | 8 B out `crkey` | R:1004-1015 |

RTYPE constants: Write Exclusive = **1** (D0 flat volume, R:832); Write Exclusive –
Registrants Only (WERO) = **3** (R:844; it shipped as 2 = Exclusive Access until
`fix/wero-rtype` — a registered second host was refused READS; pinned by TW:80-116).

### 1.3 Procedures, step by step, with errno handling

**Register** (`register`, R:1091-1112): try `RREGA 0 | IEKEY(0x8) | CPTPL 11b (0b11<<30)`
with `crkey = 0, nrkey = key`; on Reservation Conflict return it untouched; on any OTHER
error retry `0x8` (IEKEY, no CPTPL) then `0` (plain) — "targets vary on IEKEY/CPTPL support".
Idempotent for an already-registered same key (both fakes and nvmet).

**The register ladder** (`register_ladder`, R:600-706 — charter §8 verbatim):
1. plain `register(key)`. Success ⇒ `Registered`; if a cap is LEARNED, read the report and
   `unlearn_registrant_cap_on_success(regctl)` (R:605-609, 410-420).
2. A non-conflict error: if it is a DEVICE-ANSWERED status in the command-specific (SCT 1,
   `0x100..0x1FF`) or vendor-specific (SCT 7, `0x700..0x7FF`) class and not transient
   (R:358-365; transient = aborts `0x07-0x0a`, `0x21`, `0x22`, Namespace Not Ready `0x82`,
   any path-related `0x300..0x37F` — R:313-351) ⇒ read the report and
   `learn_registrant_cap(status, regctl)` (§1.7); return the error either way (R:612-636).
3. Conflict: read **FID 0x81** (`wire_host_id`); empty or unreadable ⇒ fail CLOSED with the
   original conflict (R:645-661). Read the report; failure ⇒ fail closed (R:662-671).
4. `own_stale = registrants where host_id == ours ∧ rkey ≠ key` (R:672-677). Empty ⇒ fail
   closed, logging "a foreign registration is not ours to remove" (R:678-686).
5. `unregister(stale)` for each (`RREGA 1, crkey = stale, nrkey = 0` — device-validated
   against THIS host's registration, R:1114-1118); any failure is wrapped naming the original
   conflict (R:688-696). Then `register(key)` fresh (R:697-702) ⇒
   `RecoveredOwnStale{unregistered}`; the gate logs it as a crashed incarnation's residue
   on a spec-strict target (B:17930-17941).

WHY the ladder exists: kernel nvmet's IEKEY register REPLACES this host's stale key in place
(lenient); SPDK v26.05 returns Reservation Conflict for a different-key re-register from the
same host, which bricked kill-9 → remount there (R:23-32, 1173-1186).

**Acquire** (`rsv_acquire`, B:18406-18420): `RACQA 0`, rtype 1 (flat) or 3 (`meta_wero`,
rung 6), `crkey = key, prkey = 0`. Conflict ⇒ arbitration (§2.4 step B1).

**Preempt** (R:1128-1138): `RACQA 1` with `prkey = victim` under the same rtype; rung 6's
`preempt_and_abort_registrants_only` = `RACQA 2` rtype 3 (nvmet executes it as a plain
preempt — it queues no host I/O to abort, R:536-545).

**Report, two steps sized by REGCTL** (`report_with`, R:968-982; charter §5.5): transfer the
HEADER alone (64 B extended / 24 B short, R:148-154), read `regctl = u16 LE @5..7`, transfer
`header + stride × regctl` (stride = header length in both forms, R:157-161); if the second
image's regctl GREW, re-read once at the new size; record `bytes` for `pr_report_bytes` and
`regctl` for `pr_registrants_per_namespace` (R:977-981, 229-237). `report()` (R:1148-1161)
tries EDS = 1 first (every fabrics association has 128-bit host ids; the short form fails
with **Host Identifier Inconsistent Format, SC 0x18** on nvmet) and falls back to the short
form on any non-conflict error. Parse (R:172-211): `rtype = @4`, `regctl = @5..7`; per
registrant at `hdr + i×stride`: `cntlid u16 @0, rcsts u8 @2`; extended `rkey u64 @8,
hostid[16] @16`; short `hostid u64 @8, rkey u64 @16`; `holds = rcsts & 1 ∧ rtype ≠ 0`;
a short image refuses, a header naming more than the image holds decodes the ones that fit
(total). WHY two steps: the shipped fixed 4 KiB read broke at the 63rd extended registrant
(`(4096−64)/64`) so the 64th co-writer read as unregistered (R:137-139; TS:405).

**Release** (`resv_release`, R:1004-1022): `RRELA 0` under the HELD rtype (a mismatched type
is refused by the device), then `unregister(key)` — NVMe release does not unregister, and a
stale registration makes this host's next fresh-key register conflict (observed on nvmet in
the M1 session; refused outright by SPDK).

### 1.4 Host identity — `SQUEEZEFS_HOSTNQN` / `SQUEEZEFS_HOSTID`, pair-or-neither

Two identities, deliberately distinct (R:484-495):
- `host_identity()` — the CONFIGURED pair, read from `/etc/nvme/hostnqn` + `/etc/nvme/hostid`
  (trimmed; absent files read as empty, R:1041-1054). Recorded once at the gate
  (B:17904-17909) and re-read every heartbeat: a change ⇒ fail-stop "host identity changed
  under the reservation … our registration is not ours" (B:17751-17764).
- `wire_host_id()` — the device's answer (FID 0x81). The ONLY value matched against report
  registrants (the register ladder, `registrant_identity_shared`); never falls back to
  `/etc/nvme` because the 2026-07-17 SPDK session measured the two diverging (R:1065-1069).

The knob pair (`INIT:46-48`, env_knobs.rs:142-143): explicit `SQUEEZEFS_HOSTNQN`/`HOSTID`
(or `-o hostnqn=`/`hostid=`, option > env) is what every fabrics connect this daemon makes
presents; **pair-or-neither** (`explicit_host_identity_from`, INIT:124-158): exactly one
configured REFUSES with "per-mount host identity is PAIR-OR-NEITHER (KD-MW-3): hostnqn is
configured … but hostid is not — set … or unset both. A hostnqn presented under the host's
default hostid is how registrants alias on the device" (and the mirror text); a present
`-o` key with an empty value is malformed (INIT:97-112). Defaults (INIT:50-91): read
`/etc/nvme/hostid`, else mint a v4 uuid AND WRITE it to `/etc/nvme/hostid`; hostnqn
likewise, default `nqn.2014-08.org.nvmexpress:uuid:<hostid>`. Rung 5 only READS the pair
for the identity-stability check; presenting it on fabric connects is rung 8's initiator.

### 1.5 The heartbeat re-check (`guard_heartbeat`, B:18741-18843) — PR half

Every 10 s beat, BEFORE the claim refresh ("never refresh a claim past a foreign
reservation", B:18749):
1. `host_identity()`; mismatch vs recorded ⇒ `guard_fail_stop` (B:17751-17764); unreadable
   ⇒ warn, skip this beat (B:18766-18773).
2. `report()`: holder == our key ⇒ fine. **holder == None ⇒ PTPL lapse**: the target
   dropped the reservation (power cycle / `nvmet` restart — nvmet has NO PTPL, charter §8)
   and nothing took it ⇒ `register_ladder` + `rsv_acquire`; success counts
   `writer_guard_pr_reacquires` and logs "reservation holdership had lapsed (PTPL-less
   target power cycle?) — re-acquired … audit the fabric" (B:18776-18818); failure ⇒
   fail-stop "re-acquire after reservation lapse failed … another mount may have taken the
   namespace" (B:18819-18825). **holder == foreign ⇒ fail-stop** "reservation is held by a
   foreign registrant (key …) — this holder has been usurped" (B:18827-18833). Report error
   ⇒ warn, retry next beat (B:18835-18841).
Residual stated by the design: ≤ 10 s after a target power cycle in which a second mount
could acquire before the incumbent's re-check — detection-grade for that window
(DMT:181). `writer_guard_pr_reacquires` growth across target restarts is EXPECTED
(charter §8), so it is a counter, not a tripwire.

### 1.6 The fence latch — "a reservation-conflict errno at ANY barrier latches FAILED"

Reference (`note_barrier_failure`, B:16138-16157; design DMT:173): classify the barrier's
`io::Error`; `EBADE` (`is_reservation_conflict`, R:466-469 — the block layer's mapping of
`BLK_STS_RESV_CONFLICT`) ⇒ `guard_fail_stop("reservation-conflict errno at a durability
barrier — this holder's writes are fenced at the device (usurped or paused-then-preempted)")`
= `writer_guard_fenced += 1` and `failed` latched AT ONCE (B:18723-18732); every other
errno ⇒ the generic rung (`JOURNAL_FAILURE_LATCH = 3` consecutive, success-reset, on a
counter SEPARATE from `journal_failures` because the entry-write success path resets that
one right before the strict barrier — B:16043-16048, B:241). Both the strict `commit_tx`
barrier and the checkpoint tick's barrier share the classifier (B:16024-16028). Measured
caveat (B:16034-16042): nvmet 7.1.3 + BUFFERED writeback can collapse the conflict to plain
`EIO` by `fdatasync` (`mapping_set_error`), so the generic rung is the stated fallback.
Detection bound: ≤ one flush cadence + one barrier (50 ms default; immediate in strict).
Acked-but-unbarriered coalesced-mode transactions of a fenced holder are LOST — the same
class as any device failure inside the coalesced window (DMT:173).

**Where it plugs into the port.** The port's writes are O_DIRECT (charter §5.1), so the
conflict can arrive on an ENTRY WRITE CQE as well as on the barrier; both sites must latch:
- `journal.FailureLatch` (journal.zig L1064-1111) grows a third arm: `noteFence()` =
  `failStop()` + `guard.stats.fenced.inc()` + the guard log line — FAILED at once, no run.
- `journal_lane.runVerdicts` (L1230-1247): `if (c.first_err == error.ReservationConflict)
  latch.noteFence()` before `noteJournalFailure`; `barrierDone` (L1362-1367): the same
  check before `noteFailure(.barrier)`. `errors.fromCqeRes(-52)` already yields
  `error.ReservationConflict` (errors.zig L159).
- `checkpoint_cycle.syncDevice` (L547-549) and the device sinks for ledger/bitmap/node
  writes (`device_loader` hooks) classify the same way — the reference names "checkpoint
  node/ledger writes" as fence sites (DMT:173).
- The mount-wide lattice: every data `Device` gets a `FenceSignal` (nvme_derive.zig L421)
  whose `poll` reads `OR(meta volumes' FailureLatch.isFailed())`, so a fenced metadata
  holder stops its data DMA too (`data_custody` module doc, DC:43-50: "the D0 fail-stop
  lattice is mount-wide"); `classifyDmaOutcome` (nvme_dev.zig L1239-1245) already latches
  the data side on its own EBADE.
- `guard_fail_stop`'s message and `writer_guard_fenced` are per volume (B:1636-1639).

---

## 2. Layer B2 — the `writer_claim` record and the D0 gate

### 2.1 The record (B:746-851)

Key: xattr `writer_claim` on **local ino 1** (`WRITER_CLAIM_XATTR`, B:751) — stored as an
ordinary xattr record `ino 1 ‖ 03 ‖ hash56(name, hash_seed) ‖ coll` through `xattr_slot`
(B:24651-24678: scan the `(ino, hash56)` chain for the name, else the first free coll).
PER-VOLUME control state: it never travels with a migrating slot (B:987-990 excludes its
`(ino 1, hash56)` from the VL5b tee; B:18666-18670 ignores one found in a guest keyspace),
is hidden from FUSE by the reserved-xattr screen (B:740-744; TM:2731), and is excluded from
the digest walk (builder.rs:1127-1134).

**Port (decision 0003 §AS; `dynamic_routing.md` Q4):** the record is the tree-0 control record
`TREE_CONTROL ‖ tag 15` (`control_records.writerClaimKey`), value = the 88-byte `WriterClaimV1`
alone. At local ino 1 the key routed to slot 0, which only member 0 hosts: on an armed set the
commit door refused every refresh and the dismount's delete of a member ≥ 1 (`SlotBusy`). Tree 0
has no slot, no door, no migration and no digest, so the tee/digest/FUSE exclusions are moot.

Value (reference): compact JSON `{"id","ts","pid","boot"[,"term"]}` (B:800-812):
- `id` — `writer_id`, a fresh uuid v4 PER MOUNT (B:2088; `inner.writer_id = Uuid::new_v4()`),
  also the `client:{id}` suffix's sibling. Not the node identity (that is
  `node_{16 hex}.m{8 hex}`, MI:1-30 — rung 6's durable member id).
- `ts` — unix SECONDS (`unix_now_secs`, B:17379-17384), the heartbeat.
- `pid` — `std::process::id()`.
- `boot` — `/proc/sys/kernel/random/boot_id`, trimmed; empty when unreadable (`read_boot_id`,
  B:17372-17377). Scopes the pid proof to one boot (B:780-782).
- `term` — DLM S2 era, omitted when 0 (rung 6; B:784-795).
Decode is tolerant (`get(..)?.as_*()?`, B:820-829); unparseable ⇒ `None` ⇒ treated as a STALE
FOREIGN claim, never auto-taken (B:814-816).

**The PR key** = `xxh3_64(writer_id ‖ NUL ‖ boot_id)` (`derive_pr_key`, B:846-850); set
at open (B:2096) from the same two words the record carries, so a joiner can cross-check a
report's holder against the durable claim (B:836-844).

**Port shape (0002 rows 2/4 precedent):** `extern struct WriterClaimV1 { magic u32 LE =
"WCLM", version u8 = 1, kind u8, pad u16, id [16]u8 (uuid bytes), boot [16]u8 (boot_id
uuid bytes, zero = unreadable), ts u64 LE, pid u32 LE, term u64 LE (reserved 0 until
rung 6), xxh3 u64 }` — 72 B, `comptime` size-asserted, canonical total codec; the
operator-facing text form is rendered by the CLI, not stored. Same for `client:{id}` (§3.1).

### 2.2 The ordering law (B:1897-1916, 1971-2146; DMT:204-205)

`open_writer`, in the pinned order (trace events `flock_acquired → claim_committed →
claim_barriered → checkpoint_task_spawned`, B:1807-1810, 18604-18614; pinned by T:339-358):
1. Layer A flock (B:1978; §2.6).
2. DUR-5 superblock self-heal under the flock (B:2036-2047 — the port's `backend.open` step 3
   already classifies both copies; the rewrite of sector 0 belongs here).
3. Bootstrap replay `open_inner(Writer)` (B:2080; reads `boot_id`, B:2877) — the port's steps
   2-8.
4. `writer_id = uuid v4`; `reservations = resolve_for_mount(path)` on the blocking pool
   (B:2088-2095); `pr_key` (B:2096); fence posture (B:2102, rung 6).
5. **The gate** `writer_guard_gate` (B:17833-18066): classify (§2.3) → B1 register ladder +
   acquire/arbitrate (§2.4) → `preclaim_ring_recovery` (B:18013-18029: inline barriered
   checkpoint cycles until one max-size user entry admits — the checkpoint task does not
   exist yet, so a parked claim would have no drain source) → `resolve_writer_term`
   (rung 6) → `commit_claim_tx` (B:18075-18103: ONE `KvTx` under the exclusive `I{1}` guard,
   `stage_put(TREE_XATTRS, claim_key, value)`, through the conveyor) → `claimed = true` →
   `sync_device()` **barrier** → publish the term.
   A gate refusal: `release_reservation()` then `drop(guard_fd)` SYNCHRONOUSLY before
   returning (B:2114-2129): a failed claim COMMIT already spawned the conveyor pass task whose
   per-iteration backend upgrade can pin the flock past the error return (the 2026-07-27
   torn-claim remount flake; T:1585-1600).
6. `writer_bring_up` (B:2132-2142, 2149-2190): prime frame stamps, restore own-residue pools,
   `cover_bring_up_residue` BEFORE the volume serves (the claim tx must not sit
   committed-but-uncovered — the D1.b wedge-crumb), the appender JOIN (rung 6), cover once
   more.
7. `spawn_checkpoint_task` (B:2144; emits `checkpoint_task_spawned`, checkpoint.rs:1583) and
   the times-drain task. Return; FUSE arms later in `main`.

**Port insertion point** (`backend.zig` `open`, L310-591): the port spawns the lane
(L509), the checkpointer (L528), runs `coverBringUpResidue` (L557), the conveyor (L563) and
`startPass` (L584), then the checkpoint THREAD (L586). The gate goes between `startPass`
and `ckpt_thread = spawn`: the claim tx is committed through `be.cv` (the pass fiber is
live), barriered through the lane's fsync (`laneBarrier`), THEN `coverBringUpResidue` runs
(move it after the gate, or run it a second time — the reference covers after the claim,
B:2159-2164), THEN the checkpoint thread spawns. The ring-recovery preflight is the port's
`alignRingHead` + a guarded-cycles loop on `be.cp` before the commit. `errdefer` order:
release PR → close `guard_fd` (the reverse of acquisition).

### 2.3 Classification (`classify_claim`, B:17795-17828; `ClaimEvidence` B:17576-17586)

Input: the replayed raw value (RAM state after bootstrap replay — the staleness evidence,
DMT:191-192) and `now` (unix seconds).
```
absent                                        → Reclaimable   (first guard-aware mount)
undecodable                                   → StaleForeign(None)   (never auto-taken)
boot == ours ∧ pid == ours                    → Reclaimable   (own residue: drop-without-shutdown)
boot == ours ∧ pid_provably_dead(pid)         → Reclaimable   (dead-pid proof; ANY age — kill -9 leaves a FRESH claim; "no wait ever")
age ≤ CLIENT_STALE_TTL_SECS (45)              → FreshForeign(claim)
else                                          → StaleForeign(Some(claim))
```
`same_host` requires a NON-EMPTY local boot id (B:17803) — an unreadable boot id never
proves anything. `pid_provably_dead` (B:17390-17397) = `pid ∈ (0, i32::MAX] ∧ kill(pid, 0)
== -1 ∧ errno == ESRCH`; `EPERM` (alive, foreign uid) and success are NOT proof. The same
function is the claim-set prune's (rung 6) — "one dead-holder law, never a second spelling".
`age = now.saturating_sub(ts)` (B:832-834): a future `ts` reads age 0 = fresh (conservative).

`ClaimStanding` (B:17595-17604) is the public vocabulary `{Reclaimable, Fresh, Stale}` read
through a probe (`claim_standing`, B:17785-17792) by the symmetric join target
(`live_manager_present`, mod.rs:708-716: local exclusive flock holder OR a Fresh claim — rung
6) and the `-o ro` quiescent reader (rung 6).

### 2.4 The gate's decision matrix (`writer_guard_gate`, B:17862-17900)

| Evidence | PR substrate | Verdict | Refusal text (B:) |
|---|---|---|---|
| `FreshForeign(c)` | any | **refuse** | "`{path}`: metadata volume is claimed by a live writer (claim: id=…, pid=…, boot=…, age=…s) — concurrent mounts of one metadata volume are refused (single-writer guard)`{sym note}`. A crashed holder on THIS host is reclaimed automatically once its pid is provably dead; otherwise stop that writer or wait for its claim to expire (ttl 45s)" (17866-17876) |
| `StaleForeign(_)` | **none** | **refuse** | "`{path}`: stale writer claim (claim: …)/(unparseable claim value) from another holder — automatic cross-host takeover is disabled on volumes without NVMe Persistent Reservations (a paused holder cannot be detected); verify the holder is down, then run `squeezefs claim clear <sqmeta-uri>` (operator attestation). The claim is never auto-taken" (17882-17895) |
| `StaleForeign(_)` | PR | proceed → B1 arbitrates (PREEMPT) | — |
| `Reclaimable` | any | proceed | — |

`{sym note}` = `symmetric_second_writer_note()` (B:17346-17350; rung 6 appends it).

**B1 arbitration inside the gate** (B:17902-17978): `host_identity` recorded (unreadable ⇒
warn "identity stability cannot be verified this session", B:17910-17916) → `register_ladder`
(error ⇒ `pr_error("reservation register", e)` = Busy "`{path}`: reservation register
failed on the PR-capable namespace: {e} (single-writer guard)", B:18519-18524) →
`rsv_acquire`: Ok ⇒ done; **conflict** ⇒ `report()`; no holder in the report ⇒ refuse
"reservation acquire conflicted but the report names no holder — refusing to arbitrate blind
(single-writer guard)" (B:17953-17958); else `preempt(key, victim)` (rtype 1; WERO form on
rung 6) and warn "preempted stale reservation holder key {victim:#018x} (device-fenced
takeover; writer_claim evidence was stale/absent)" (B:17970-17974). Any other acquire error
⇒ `pr_error("reservation acquire")`. Then `pr_active = true` and the rung-6 shared-identity
probe (`registrant_identity_shared`, R:117-129: another rkey under OUR wire host id ⇒
`pr_registrant_shared = 1` + the loud "fencing between the two is PROCESS-LOCAL" warning,
B:17987-18010). On PR volumes the acquire is what arbitrates two mounts that both read an
absent/stale claim: exactly one acquire succeeds (DMT:203).

### 2.5 Heartbeat, staleness, clean unmount

- **Heartbeat task** (FC:31913-31951): `spawn_meta("writer_guard_heartbeat")`, an interval
  of `CLIENT_HEARTBEAT_INTERVAL_SECS = 10` (FC:191); every tick: stop-latch check (a
  post-unmount beat must never touch a released guard, FC:31926-31931) →
  `refresh_client_registration` (§3.1) → per volume `guard_heartbeat`: B1 re-check (§1.5)
  then the B2 refresh — `setxattr_internal(1, "writer_claim", encode{id, ts = now, pid,
  boot, term = writer_term()})` (B:18851-18869); skipped when no guard fd, read-only,
  failed, shutting down (B:18742-18747) or when the manager role was released (rung 6,
  B:18851). Best-effort: a failed refresh only warns (B:18865-18868). The refresh is an
  ordinary xattr tx that rides the conveyor and the COALESCED barrier (strict mode barriers
  per commit); `token_quiet` (B:1033-1039, rung 6) so the pass recalls no reader's root token
  for it. Heartbeat refreshes may NOT blind-write past takeover evidence: on PR volumes the
  device is the authority (the next barrier fences), on non-PR a resumed paused holder cannot
  observe the usurper — which is WHY automatic cross-host takeover is disabled there
  (DMT:207).
- **Staleness**: `CLIENT_STALE_TTL_SECS = 45` (FC:195) — the ONE staleness law shared by
  `client:*`, `writer_claim`, the `format` preflight and (rung 6) `T_owner`.
- **Clean unmount** (`shutdown`, B:16670-16848): delete the claim exactly once, FIRST, while
  the write gate is open (`claimed.swap(false) ∧ !failed` ⇒ `removexattr_internal(1,
  "writer_claim")`; failure warns "it will age out by TTL", B:16752-16762); the final
  checkpoint makes the deletion durable; `release_reservation()` (release + unregister, once,
  best-effort, B:16845, 18528-18552) after the final barrier; `drop(guard_fd)` LAST
  (B:16846). A fenced/failed volume cannot write: the claim ages out (B:16752). `kill -9`
  skips both by construction (B:16668-16669).
- **Multi-volume sets** (`open_meta_volume_set`, mod.rs:284-316): claim every volume in SET
  ORDER; a refusal on volume k runs `shutdown()` on `0..k` (flocks, claims, PRs released) and
  propagates k's error (pinned T:1481).

### 2.6 Layer A additions on top of the landed flock (B:17978-2034, 17694-17766)

On `EWOULDBLOCK`: read the claim best-effort through a probe (B:17770-17774) for the message;
**transient-holder absorption** (`await_transient_flock_release`): poll `flock(LOCK_EX|LOCK_NB)`
every 5 ms for ≤ `TRANSIENT_FLOCK_WAIT = 2 s` (B:17304, 17719) iff the holder is (1) THIS
process (pid + boot: a drop-without-shutdown teardown pin — the detached pass/checkpoint
tasks each pin the backend one pass), (2) a provably-dead same-host holder under a transient
`LOCK_SH` probe (rung-9 finding #5: readers' released-immediately probes while the authority
is dark), or (3) ANONYMOUS (no claim at all — udevd's `BLOCK_DEVICE_LOCKING` change-event
flock after every write-close of the node, released in ms; the mw_fleet `--owners` finding
2026-08-23). A live claimed foreign holder refuses INSTANTLY. At the bound, re-probe once
(a concurrent mount inside its pre-claim window has committed by now) and refuse Busy:
with a claim — "`{path}`: another squeezefs process holds the writer lock (claim: id=…,
pid=…, boot=…, age=…s) — concurrent mounts of one metadata volume are refused (single-writer
guard)"; without — "`{path}`: the writer lock is held by a process that left no on-volume
writer_claim and did not release within 2s — squeezefs writers commit their claim within
milliseconds of taking the lock, so the holder is an external flock on the device node
(udevd's change-event probe releases quickly; this one did not). Concurrent mounts of one
metadata volume are refused (single-writer guard)" (B:2005-2027). The reader's probe
`flock(LOCK_SH|LOCK_NB)` then `LOCK_UN` at once (`probe_shared_lock`, B:17655-17692) is
classification only — a RETAINED shared lock would refuse a legitimate same-host writer
(B:17636-17646; rung 6's `-o ro`). The port reads the claim for the message only and
absorbs by the LOCKER `/proc/locks` names (`guard.proc.flockExclusive`, decision 0002 row
542): this process or a process of this executable refuses at once; any other locker —
udevd's `LOCK_SH` rules and its `LOCK_EX` + `BLKRRPART` after every write-close of a whole
disk (strace, systemd 259) — is polled every 5 ms for ≤ 5 s, then refused.

### 2.7 The recovery ladder (charter §5.7; DMT:207, 225 — "in that order of automation")

1. **Same-host dead-pid proof** — automatic, instant, any age (§2.3).
2. **PR preempt** — automatic on a PR substrate for a TTL-stale/absent claim (§2.4).
3. **TTL** — 45 s makes a foreign claim `Stale`; on PR it enables 2; on non-PR it only
   unlocks 4.
4. **`squeezefs claim clear <sqmeta-uri>`** (`claim_clear`, B:18878-18977; MAIN:5349-5390):
   per volume of the URI: take the flock (`Held` ⇒ refuse "`{path}`: refusing to clear the
   writer claim — the volume is live-mounted on this host (the writer lock is held). Unmount
   it first", B:18880-18892) → `open_inner(Writer)` → sweep `client:*` on ino 1: any
   heartbeat-fresh one ⇒ refuse "refusing to clear the writer claim — the volume has live
   client registrations: [..]" (B:18904-18929) → no claim ⇒ `NoClaim` ("no writer claim
   present — nothing to clear") → a FRESH claim ⇒ refuse "refusing to clear a FRESH writer
   claim (claim: …) — the holder heartbeated within the 45s ttl and may be alive. Stop that
   writer (or wait for the claim to expire), then retry" (B:18939-18950) → stale or
   unattributable ⇒ `writer_bring_up` → `removexattr_internal` → `sync_device` →
   `checkpoint_now` → `shutdown` ⇒ `Cleared(holder)` (unparseable ⇒ a placeholder holder
   `id = "<unparseable claim value>"`, B:18970-18976). CLI prints "cleared stale writer
   claim (holder id=…, pid=…, boot=…, last heartbeat ts=…). This was an operator attestation
   that the holder is down — if it was merely partitioned, stop it before it reconnects."
   and exits nonzero if any volume refused (MAIN:5363-5389). **No bypass knob exists**
   (DMT:225: "There is no mount flag to bypass the guard").

Rung 6 adds `appender clear` on the same pattern (charter §5.5).

### 2.8 `writer_guard_mode` (B:18563-18586)

`"reader"` (`-o ro`, decided by CAUSE not lock state) | `"joined-appender"` (rung 6) |
`"unguarded"` (no guard fd — probes, never in stats) | `"flock+pr"` (reservations resolved) |
`"flock"` (guarded, no PR, degraded read-only by unknown ro bits) | `"flock+claim"`.
Operators alert on fleet drift (DMT:414).

---

## 3. The clients / claims / heartbeats plane

### 3.1 `client:{uuid}` registrations (rung 5)

- Written at mount and every 10 s by `refresh_client_registration` (FC:16295-16344):
  `setxattr(1, "client:{client_id}", {"ts":<secs>,"pid":<pid>[,"job_endpoint":"ip:port"]})`;
  `client_id` = a uuid v4 minted per mount (FC:31912). Skipped on a read-only mount (a reader
  writes nothing; `squeezefs clients` does not list readers until the membership plane —
  FC:16296-16306) and by a joined appender (rung 6, FC:16320-16331). `job_endpoint` is the
  job fabric's discovery field — rung 8.
- Deleted at dismount (`removexattr(1, "client:{id}")`, FC:26150-26156); the process waits
  for the teardown bounded so records deregister instead of lingering to the 45 s TTL
  (FC:32370-32389).
- Routing: `client:` records are ROUTED records on GLOBAL ino 1 (slot 0's volume); after a
  slot-0 migration they live in that volume's guest keyspace `guest_local_ino(0, 1)`, so the
  reader scans both roots (B:18641-18657; rung 5's dynamic routing).

### 3.2 `mount_registrations` — the ONE reader (B:18638-18719)

For every `client:*` and `writer_claim` key on the root(s): `heartbeat_ts = parse ts`,
`age = now − ts`, `heartbeat_fresh = age ≤ 45`; writer rows add `boot` and
`holder_provably_dead = same_host ∧ pid_provably_dead` (client rows: `false` — they carry no
boot, B:18703). `MountRegistration.state()` (B:903-911): `dead` (provably dead — reclaimable)
> `live` (fresh) > `stale`. JSON shape (B:915-927): `{kind, key, id, pid, boot,
heartbeat_ts, age_secs, state, job_endpoint}`.

### 3.3 Verbs

- **`squeezefs clients <sqmeta-uri|path> [--json]`** (MAIN:206-218, 4641-4648,
  8337-8491): `open_probe` per volume (never blocked by a live mount), rows from
  `mount_registrations` plus (rung 6) the membership census; text header `KIND ID PID STATE
  AGE VOLUME`, rows `{kind:<7} {id:<38} {pid|-:<8} {state:<6} {age}s|-:<5} {volume}`, footer
  "{n} registration(s): {live} live, {stale} stale, {dead} dead (reclaimable)."; empty ⇒ "No
  client registrations on {n} metadata volume(s) — no mount (live or crashed) holds this
  filesystem."; JSON `{volumes, clients:[row+"volume"], live, stale, dead, staging_slots}`
  (the staging-slot residue rows are rung 7/8's). `status`'s `"Clients"` emits the same row
  JSON (B:913-914).
- **`format` preflight** (builder.rs:1200-1226): the live sweep is `mount_registrations`
  filtered on `heartbeat_fresh` — `client:* ∪ writer_claim` under the one law; refuses
  "Cannot format: metadata volume is actively mounted by clients: [..]" even with `--force`
  (format_guard_tests.rs:134, 149).
- **`claim clear`** — §2.7.

### 3.4 The membership plane (rung 6 — recorded here so rung 5 reserves nothing wrongly)

Why (M:7-20): per-beat `client:` xattr commits serialize on ONE volume (ino 1 → slot 0) at
455 beats/s vs the 1,500/s 15 k clients need; the read side is `listxattr(1)` + a `getxattr`
per client under `I{1}`. Mechanism (M:22-49): ONE durable rendezvous record
`membership_owner` on local ino 1 at the owner's arm (`{v, id, term, endpoint,
owner_claim_id, ttl_ms, ts, pid, boot}`, M:553-645; `is_fresh` = armed within its own
`ttl_ms`, M:593-600) + the durable `claim_set` (incompat bit 14, members `{id, role, pid,
boot, endpoint, pr_key, ts}`, rewritten on CHANGE only; un-engaged volumes read the singleton
PROJECTION of `writer_claim`, M:239-393); liveness = lease renewal over `cluster_wire`, the
lease table RAM-only (`MembershipOwner`, M:1269-1289: `scc::HashMap<id, MemberState{role,
endpoint, pid, boot, epoch, join_seq, joined_ms, renewed_ms, deadline_ms, acked_free_epoch,
mount, pr_key}>`). Wire (MW:184-210): verbs `0x0100 JOIN, 0x0101 RENEW, 0x0102 LEAVE,
0x0103 CENSUS` (a BLOCK so parallel stages cannot collide), statuses `0x11 REFUSED, 0x12
UNKNOWN_LEASE, 0x13 BAD_REQUEST`, census paged at `CENSUS_PAGE_MAX = 1024` rows (a 15 k
census at ~120 B/row is ~1.8 MB, past the control frame cap); `JoinRequest{id, role,
endpoint, pid, boot, prior_epoch, pr_key, mount}` (M:913-936); `Grant{epoch, term,
t_owner_ms, skew_max_ms, d_purge_ms, renew_ms, granted_at_owner_ms, checkpoint_ceiling_ms,
slot_leases_ack, slot_release_notices, offered_slots}` (M:940-995). Owner: `arm` REFUSES
`term ≤ prior_term` (M:1326-1340); `expire_due` evicts at `now ≥ deadline_ms` = `T_owner`
(M:2008-2033); `evict` declares the dead epoch, counts `membership_evictions`, notes
departure and death → the death ledger `RecordDeath` carrying `pr_key` (M:1961-2001); the
grace window (M:1244-1263, 2039-2130). Member: `MemberSession` over `MemberLeaseWords`
(§3.5), renewal on the dedicated `sqz-lease-io` thread (MW:46-55), self-fence at `T_self`
poisons data custody (TM:518-575). Armed only when `SQUEEZEFS_MEMBERSHIP_BIND ≠ off` AND a
`job:enroll` secret exists (M:3017-3055); the symmetric join ladder arms it at `auto`
(M:3011-3016). Identity `node_{16 hex}[.m{8 hex}]` (MI:14-49; node token from
`SQUEEZEFS_NODE_ID_FILE` → `/etc/machine-id` → `/var/lib/dbus/machine-id` →
`/etc/squeezefs/node-id`, writer_scope.rs:312-342; refuses loud rather than synthesize).
`clients` reads the census as registrations (MW:853; MAIN:8361-8375) — the only way READERS
appear.

### 3.5 The lease clock core (LC:1-344; loom LOOM:5557-5680) — port the arithmetic + model in rung 5

Law (LC:51-57): `T_self = T_owner − (2·skew_max + D_purge)`, `None` (REFUSE, never clamp)
when the reserve `≥ T_owner`; `renew_interval = clamp(T_self/3, 1 ms, shipped_beat 10 s)`
(LC:62-64: three attempts before the member's own deadline, never a regression below the
shipped beat); grant-side `t_self_ms` saturates to 0 = a hostile/garbled grant fail-stops at
once (LC:70-72). Derivation (`LeaseClocks::derive`, M:832-863): `T_owner =
SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS` default `45 000`; `skew_max = max(T_owner ×
MONOTONIC_RATE_DRIFT_PPM/1e6, observed RTT)` with `MONOTONIC_RATE_DRIFT_PPM = 500` (two
oscillators at ±500 ppm over temperature ⇒ 22.5 ms at 45 s — a PHYSICAL bound, M:774-782);
`D_purge = max(2 × revalidation cadence, RTT)` (observe, then finish); `grace = T_owner`.
Refusal text (M:879-886): "membership lease clocks refuse to arm: T_self = T_owner −
2·skew_max − D_purge = … is not positive, so a member could still believe it holds custody
the owner has already re-granted (spec §6.7 'Two lease clocks'). Raise
SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS, or lower SQUEEZEFS_MEMBERSHIP_SKEW_MAX_MS /
SQUEEZEFS_MEMBERSHIP_PURGE_MS". Skew handling: the member anchors on its own SEND instant
(never the owner's clock value), so its deadline precedes the owner's by ≥ `2·skew_max +
D_purge + RTT` (LC:74-79; theorem test LC:330-343).

`MemberLeaseWords` (LC:80-262): atomics `epoch, t_self_deadline_ms, renew_at_ms, renew_ms,
acked_free_epoch, learned_label, learned_at_ms, skew_max_ms, d_purge_ms, fenced`.
`renewed` publishes the ANCHOR before the LABEL (both Release) and guards on `label >
learned_label` (LC:137-175); `learned_label()` reads label first (Acquire) then anchor
(LC:202-205); `fence()` = `!fenced.swap(true, AcqRel)` (LC:252-254); `self_fence_due(now) =
!fenced ∧ now ≥ deadline` (LC:242-245); `renewed_carriage` only brings `renew_at` FORWARD
(`fetch_min`) and learns no label (LC:180-199). **What the loom model asserts**
(`lease_clock_models`): (1) `renewal_label_never_pairs_with_the_old_anchor` — a reader on a
SPAWNED thread (load-bearing: loom 0.7 on the main thread never exhibits the guarded
writer's skew) observing `NEW_LABEL` must read `NEW_ANCHOR`; `OLD_LABEL` may pair with
either (the conservative direction). **Weakenings** (verified 2026-08-15): flipping the two
stores, or relaxing the label's Release/Acquire pair to Relaxed, admits `(NEW_LABEL,
OLD_ANCHOR)` (LOOM:5577-5644). (2) `self_fence_side_effects_run_exactly_once` — two racing
`fence()` calls yield exactly one `true`; **weakening**: load-then-store lets both claim
first (LOOM:5646-5680). The port's `src/model/explorer.zig` model: a plain value type with
per-thread PCs, one atomic action per step, store buffering modelled for the Release stores
(`expectHolds` + the two `expectFails`).

---

## 4. Knobs, gauges, constants

### 4.1 Knobs (reference row → port class per `knobs.Class`)

| Knob | Kind / range / default | Reference | Port class, rung |
|---|---|---|---|
| `SQUEEZEFS_HOSTNQN`, `SQUEEZEFS_HOSTID` | str; default = `/etc/nvme/{hostnqn,hostid}`; pair-or-neither; `-o hostnqn=/hostid=` wins | env_knobs.rs:142-143; INIT:124-158 | `config`, rung 5 reads for identity stability; the pair law in a pure `resolveHostIdentity(env, opts) → {none, pair, refusal}` |
| `SQUEEZEFS_PR_REGISTRANT_CAP` | int 1..=65535; unset = learn from the device (nvmet: unbounded) | R:249-253, 376-381 | `derived_override`, rung 6 (ships with the primitive, read by `registrant_cap_gate`) |
| `SQUEEZEFS_META_PR_WERO` | bool, default 1 | env_knobs.rs:419; B:18460 | rung 6 — see quirk Q8 (delete) |
| `SQUEEZEFS_SYM_ALLOW_NON_PR` | bool, default 0; LOUD lab opt-in | env_knobs.rs:418; B:18459 | rung 6 — see quirk Q8 |
| `SQUEEZEFS_MEMBERSHIP_BIND` | str `off|auto|addr:port`, default off; malformed refuses | env_knobs.rs:352; M:2832-2850 | `enum_select`/`config`, rung 6 |
| `SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS` | int 1 000..MS_MAX, default 45 000 | env_knobs.rs:353 | rung 6 — see quirk Q9 |
| `SQUEEZEFS_MEMBERSHIP_SKEW_MAX_MS` | int 1..60 000, derived `max(T_owner × 500 ppm, RTT)` | env_knobs.rs:354 | `derived_override`, rung 6 |
| `SQUEEZEFS_MEMBERSHIP_PURGE_MS` | int 0..MS_MAX, derived `max(2 × cadence, RTT)` | env_knobs.rs:355 | `derived_override`, rung 6 |
| `SQUEEZEFS_MEMBERSHIP_GRACE_MS` | int 0..MS_MAX, derived `= T_owner`; 0 = no window | env_knobs.rs:356 | `derived_override`, rung 6 |
| `SQUEEZEFS_MEMBERSHIP_RENEW_LANE` | bool, default on (dedicated `sqz-lease-io` thread vs the shared pool) | env_knobs.rs:357; MW:46-55 | `ab_lever`, rung 6 |
| `SQUEEZEFS_NODE_ID_FILE` | path | writer_scope.rs:316 | `config`, rung 6 |
| `SQUEEZEFS_META_FLUSH_INTERVAL_MS` | (exists, knobs.zig L109) — the fence detection bound's cadence term | B:16036 | landed |

No knob tunes the heartbeat (10 s), the TTL (45 s) or the transient-flock bound (2 s); no
knob bypasses the guard. Test seams the reference used (not knobs): `install_override` for
the fake client (R:724-729), `uring_fs::arm_barrier_error(path, EBADE)` (uring_fs.rs:1316),
`TEST_CONVEYOR_HOLD_*` (T:49-51), `SQUEEZEFS_GUARD_CHILD*`/`SQUEEZEFS_GUARD_VOL/OUT` (the
two-process re-exec, T:1753-1824), `SQUEEZEFS_M1_ROOT_DEV`/`_VICTIM_KEY` (root nvmet leg,
T:1673-1750). Port equivalents: a `ReservationClient` vtable slot on `OpenOptions`
(tests pass the fake), the journal lane's existing `fail_next` seam carrying `.BADE`
(`journal_lane.zig` L308-319 + `nvme_dev.setFailNextWritesErrno`), `SQUEEZEFS_TEST_GUARD_*`
rows only where a window must be held (the pass-tail pin).

### 4.2 Gauges and counters (`.stats`)

| Name | Kind | Meaning | Must-stay-0? | Reference |
|---|---|---|---|---|
| `writer_guard_mode` | per-volume string | §2.8 | — | FC:14741-14753 |
| `writer_guard_fenced` | per-volume counter | reservation-conflict barrier/write + usurpation re-check fail-stops | **yes** on a healthy fleet; "working as designed, always investigate" | B:18588-18594, FC:14755 |
| `writer_guard_pr_reacquires` | per-volume counter | PTPL-lapse re-acquires | no (expected across nvmet restarts) | B:18596-18602, FC:14759 |
| `writer_guard_term` | per-volume u64 | the durable era (rung 6; 0 now) | — | FC:14771 |
| `writer_guard_busy_refusals` | counter | mounts refused by a held flock | — | landed (`backend.zig` L150) |
| `pr_registrant_shared` | 0/1 gauge | another rkey under our wire host id | rung 6 | FC:6280, 11581 |
| `pr_registrants_per_namespace` | map ns → REGCTL | last report's count | — | R:213-247, FC:14475 |
| `pr_report_bytes` | map ns → bytes | last REGCTL-sized transfer | — | R:977, FC:14478 |
| `pr_registrant_cap` | u64 | declared, else learned, else 0 | — | R:376-381 |
| `pr_registrant_cap_refusals` | counter | joins refused at the cap | **yes on nvmet** | R:267-268, 437-439 |
| `membership_{joins, renewals, renew_refusals, evictions, self_fences, census_serves, grace_refusals, grace_reclaims, reclaim_refusals, registration_commits, renew_sched_lag_ms}`, `membership_renew_phase_ns`, `membership_renew_serve_ns`, `membership_renew_lane_calls`, `membership_shards`, `membership_mode` | counters/hist | the plane (rung 6); `registration_commits` grows per CHANGE only — growth per beat is the regression signal | — | FC:6842-6891, 11898-11928, 14379; M:649-659 |
| `meta_device_syncs` | counter | barriers | — | B:16085 (port: `ckpt.stats.meta_device_syncs`) |

Port additions this spec proposes: `writer_guard_claims_committed`, `writer_guard_heartbeats`,
`writer_guard_heartbeat_failures` (the warn at B:18865 becomes countable),
`writer_guard_reclaims_dead_pid`, `writer_guard_preempts`, `writer_guard_pr_unavailable`
(§1.1 char-node gap), `writer_guard_identity_mismatch` (tripwire — a changed hostnqn/hostid
under a live mount cannot happen on a correctly provisioned host), `clients_registered`
(gauge) — the charter's §10 engagement rule needs a gauge per mechanism.

### 4.3 Constants, with WHY

| Constant | Value | WHY (reference) | Port tie |
|---|---|---|---|
| `CLIENT_HEARTBEAT_INTERVAL_SECS` | 10 s | the one cadence for client/claim refresh, PR re-check and the fabric sampler (FC:191, 31955-31960) | a derivation tie: `TTL / 4.5` |
| `CLIENT_STALE_TTL_SECS` | 45 s | "one staleness law" (FC:195); the reference states no derivation. Defensible physical reason to put ON THE LINE: a beat stalled behind one NVMe I/O timeout (`nvme_io_timeout` default 30 s) plus one full beat (10 s) must not evict a live holder: `45 > 30 + 10`, four lost beats tolerated | tie test restates `TTL ≥ nvme_io_timeout + beat` and `TTL = 4.5 × beat` |
| `TRANSIENT_FLOCK_WAIT` / poll | 2 s / 5 ms | ms-grade shapes absorbed (udev probe, one pass pin, a released `LOCK_SH`), a live holder pays it once (B:17298-17304) | reuse the sync layer's 2 s backstop |
| `JOURNAL_FAILURE_LATCH` | 3 | consecutive generic failures (B:241; port `journal.JOURNAL_FAILURE_LATCH`) | landed |
| fence detection bound | ≤ flush cadence + 1 barrier (50 ms coalesced; 0 strict) | DMT:173, 219 | measured on the kill matrix |
| PTPL-lapse residual | ≤ 10 s | one heartbeat (DMT:181) | — |
| `MONOTONIC_RATE_DRIFT_PPM` | 500 → 22.5 ms at 45 s | oscillator spec over temperature (M:774-782) | rung 6 tie |
| Report header / stride | 64 B extended, 24 B short | NVMe Base Spec; byte-wise verified against nvmet (R:142-147) | codec size asserts |
| RESCAP byte | Identify NS byte 31 | spec | — |
| rtype WE / WERO | 1 / 3 | kernel `enum nvme_pr_type`; 2 = Exclusive Access refused a registrant's READS on nvmet 2026-08-15 (R:835-844) | TW pins |
| Reservation Conflict SC | `0x83` | spec; nvmet answers it on passthru (R:846) | — |
| `EBADE` | 52 | `BLK_STS_RESV_CONFLICT` errno (R:455-463) | landed (`errors.zig` L159) |
| SC 0x18 | Host Identifier Inconsistent Format | why EDS = 1 first (R:1152-1154) | nvmet leg |
| pr key | `xxh3_64(id ‖ NUL ‖ boot)` | one derivation for the claim and the device (B:846-850) | checksum.zig |
| measured: `commit_tx_wait` 2.198 ms saturated ⇒ 455 beats/s on one volume vs 1,500/s needed at 15 k clients | — | why per-beat xattr commits do not scale (M:11-16) | rung 6's motivation |
| measured: 63-registrant report truncation | `(4096−64)/64` | why REGCTL sizing (R:137-139) | TS:405 |

---

## 5. Tests

### 5.1 Reference contracts to port (name — one sentence; venue)

`tests/mount_writer_guard_tests.rs` (fake namespace + file volumes unless noted):
- `test_second_open_refused_while_first_holds` (T:166) — Layer A refuses a second writer open.
- `test_reopen_waits_out_same_process_teardown_pin` (T:214) — a drop-without-shutdown pin is absorbed within the 2 s window.
- `test_flock_survives_uring_fd_cache_churn` (T:258) — the lock lives on a dedicated fd, not a pool fd.
- `test_open_probe_never_blocked_by_held_mount` (T:311) — probes take no lock.
- `test_claim_committed_and_barriered_before_checkpoint_task` (T:339) — the trace order `flock → commit → barrier → ckpt spawn`.
- `test_claim_written_at_mount_and_deleted_on_clean_unmount` (T:363) — claim names this pid/boot, fresh ts; gone after `close`.
- `test_fresh_foreign_claim_refused_names_holder` (T:387) — refusal carries `{id,pid,boot,age}`.
- `test_dead_pid_proof_same_host_instant_reclaim` (T:419) — a fresh claim of a reaped pid on this boot is reclaimed at once.
- `a_transient_shared_probe_over_a_dead_holders_claim_is_waited_out` (T:454); `an_anonymous_transient_flock_holder_is_waited_out` (T:503); `a_retained_anonymous_flock_holder_refuses_honestly_at_the_bound` (T:539) — the three absorption arms and the honest refusal text.
- `test_live_pid_same_host_fresh_claim_refused` (T:581) — a live same-host pid is not proof.
- `test_stale_cross_host_claim_on_non_pr_volume_refused_names_claim_clear` (T:605) — a foreign-boot stale claim refuses naming the verb.
- `test_own_process_residual_claim_reclaimed_on_reopen` (T:638).
- `test_guard_heartbeat_refreshes_claim_timestamp` (T:654) — one beat rewrites `ts`.
- `test_claim_clear_refuses_fresh_claim` (T:683), `_clears_stale_claim` (T:709), `_no_claim_is_noop` (T:744), `_refuses_live_mounted_volume` (T:756).
- `test_pr_acquired_at_mount_released_at_clean_unmount` (T:777) — fake holder key set then cleared, zero residue.
- `test_rescap_zero_degrades_to_detection_grade` (T:807) — `writer_guard_mode == "flock+claim"`.
- `test_pr_acquire_conflict_fresh_claim_refused` (T:849); `test_pr_acquire_conflict_ttl_stale_preempted` (T:887) — `preempt_count == 1`, holder transferred.
- `test_host_identity_mismatch_fail_stops` (T:926); `test_ptpl_lapse_reacquire_counted` (T:949) — `power_cycle()` then a beat re-acquires, counter +1; `test_ptpl_lapse_foreign_holder_fail_stops` (T:978).
- Register ladder: `_fast_path_clean_namespace` (T:1077), `_recovers_own_stale_registration_after_kill9` (T:1096), `_own_stale_with_foreign_holder_leaves_holder_alone` (T:1138), `_never_unregisters_foreign_registration` (T:1174), `_non_conflict_register_error_propagates` (T:1202), `_unreadable_own_identity_fails_closed` (T:1225); `test_kill9_remount_recovers_on_spec_strict_target` (T:1253); `test_kill9_remount_on_lenient_target_stays_fast_path` (T:1296) — `unregister_count == 0`.
- `test_fence_at_barrier_fail_stop_strict_commit_path` (T:1338) — an injected `EBADE` at the barrier latches FAILED at once (not after 3) and counts `writer_guard_fenced`; later mutations stay EIO. `_checkpoint_tick_path` (T:1380) — coalesced mode latches within ~one cadence. `test_consecutive_generic_barrier_failures_escalate_with_success_reset` (T:1415).
- `test_volume_set_open_releases_guards_on_partial_failure` (T:1481) — volume A's guard released when B refuses.
- `test_torn_claim_entry_recovers_and_reclaims` (T:1521) — a torn claim entry is dropped by replay and re-claimed. `test_failed_claim_commit_releases_flock_despite_pinned_pass_tail` (T:1599) — the gate refusal releases Layer A before returning.
- `test_two_process_second_daemon_refused` (T:1777) — incident 4's shape, exit code 3 with the guard message.
- `root_session_real_nvme_reservations` (T:1673) — **real NVMe** (`SQUEEZEFS_M1_ROOT_DEV`): RESCAP, register/acquire/report/release, optional preempt leg.

`tests/writer_door_bring_up_tests.rs:164,200` — a static rail: every direct writer door
walks `writer_bring_up` (port: `tests/rails.zig` row — every `KvBackend` writer open path
calls the claim gate + cover). `tests/sym_fence_tests.rs:405,440` — a 64+-registrant report
decodes completely; the parse is total over short/malformed images (portable). TS:469, 517,
643 — cap gate declared/learned/unlearned (rung 6). `tests/wero_rtype_tests.rs:80-173` —
rtype wire values and the fake's admit law; TW:371 is a LIVE nvmet leg. `tests/
cli_clients_df_tests.rs:389,484,555` — `clients` classifies forged live/stale rows, lists a
live mount then zero after unmount, a kill -9'd daemon reads stale not active.
`tests/dlm_membership_tests.rs:492-612` — the lease clock laws (portable now); the rest is
rung 6. `tests/format_guard_tests.rs:134,149` — a live client blocks `format --force`, a
stale one does not.

### 5.2 What a FAKE NVMe seam proves on the venue's file substrates

The venue has file-backed meta volumes only. Model the device in Zig exactly as
`FakeNvmeNamespace` (R:1173-1336): shared state `{holder: ?u64, rtype, registered:
[]{host_id, key}}`, `RegisterSemantics ∈ {SpecStrict, LenientReplace}` (default
SpecStrict — the stronger law), `rescap` 0x03 or 0, `seed_holder(key)` under a synthetic
foreign host id, `power_cycle()` (clears holder AND registrations — the PTPL-less lapse),
`write_allowed(host_id)` (rtype 1 ⇒ holder's host only; rtype 3 ⇒ any registrant; unknown
⇒ false), counters `preempts`/`unregisters`. Per-host client with a mutable identity and
`wire_host_id` = the hostid's uuid bytes (else raw bytes; empty ⇒ empty) (R:1338-1383).
Acquire/preempt/release/unregister laws R:1385-1517 (an unregistered host's acquire
conflicts; re-acquire same key+rtype idempotent; rtype change conflicts; release under the
wrong rtype refused; unregistering the holder's key releases). With it every test above
except the root legs runs on a file volume. The FENCE is proven by the journal lane's
`fail_next` seam carrying `.BADE` (the port's barrier-layer injection; "injecting at the
entry-write layer would pass against wiring the real fence never exercises" — uring_fs.rs:1309-1315 —
in the port BOTH layers must be injected because O_DIRECT writes carry it too). A second
fake layer at the uring_cmd level (a `Ring128` whose CQEs are answered by the fake) proves the
72 B command encoding (opcode/CDW/NUMD/EDS/payload LE) byte-exactly without a device.

### 5.3 What needs nvmet-loop (rung 8 brings `nvmeof target`, `resv_enable=1`)

The real RESCAP byte; the `/sys/class/block/<ns>/nsid` + `ngXnY` resolution; EDS = 1 vs the
short form (SC 0x18); IEKEY/CPTPL acceptance on the register fast path; the lenient
replace-in-place register vs the SpecStrict ladder (R:1177-1186 — nvmet measured both ways
across versions, R:1095-1098); the real `EBADE` on a preempted association's O_DIRECT
write/`fdatasync`; `preempt_and_abort` as plain preempt; a target restart as the PTPL lapse
(nvmet: `resv_enable` but no PTPL — `nvmet.rs:688-693`); the shared-identity gauge with two
associations; the SPDK-strict row (no SPDK target in the venue — the fake's SpecStrict
stands in).

---

## 6. QUIRKS per decision 0002 (add a row before diverging)

| # | Reference quirk | Why poor | Better design |
|---|---|---|---|
| Q1 | `writer_claim` / `client:{id}` / `membership_owner` / `claim_set` as tolerant JSON (`serde_json`, additive-field law) on ino 1 (B:800-829; FC:16333-16341; M:295-342, 603-645) | A serialization library as an on-disk format (0002 row 2); `pid as u32` truncation; not canonical, not total in the charter's sense | `extern struct` records with magic/version/xxh3 (§2.1); the CLI renders JSON; the client record ALSO carries `boot` so the dead-pid proof applies to client rows (today `holder_provably_dead` is hard-false for them, B:18703) |
| Q2 | Control-plane PR via `ioctl(NVME_IOCTL_{IO,ADMIN}_CMD)` on the BLOCK fd from `spawn_blocking` (R:816-820, 898-923; B:18983-18995) | Charter §1.2/§12: `URING_CMD` is the one passthru path the port owns; a blocking ioctl per beat on a pool thread; the block node's uring_cmd cannot carry NVMe passthru | `IORING_OP_URING_CMD` with `NVME_URING_CMD_{IO,ADMIN}` on the `/dev/ngXnY` char fd through `uring128` (§1.1-1.2); nsid from sysfs, no `NVME_IOCTL_ID` |
| Q3 | Fence classification by `raw_os_error == EBADE` on an `io::Error` rendered through the coalescer "as strings" (B:16050-16053); the barrier-failure classifier and the write-failure one in different places | String-rendered outcomes deciding a fail-stop (charter §12 bans); two sites for one law | ONE typed `error.ReservationConflict` from the ONE errno table (landed) classified in `FailureLatch.noteFence()` at every write AND barrier site (§1.6) |
| Q4 | Dead-pid proof = `kill(pid, 0) == ESRCH` only (B:17390-17397); pid REUSE within one boot reads a stranger as "alive" and leaves a dead holder's claim to the TTL / `claim clear` | Mitigated only by the boot id + flock (DMT:225); the charter names `/proc/<pid>` | Proof = `/proc/<pid>` absent ∨ `/proc/<pid>/stat` starttime (field 22, ticks since boot) > the claim's `ts` − `btime` (a process that started after the claim's last heartbeat cannot be its author) — the port already reads `/proc/<pid>/comm` for the fleet census (0002 row 18); same boot id required |
| Q5 | The transient-flock absorption is a 5 ms poll for 2 s with literal constants (B:17304, 17719) | Free-floating constants (§1.5) | Keep the shape (an EXTERNAL holder; flock has no timed wait) but tie the bound to the sync layer's 2 s ticked backstop and name udev's ms-grade probe on the line; count `writer_guard_transient_waits` |
| Q6 | The heartbeat task is the FUSE session's (`spawn_meta` in `main`, FC:31935) while the guard it refreshes is the backend's; a `drop`-without-`shutdown` leaves the claim behind for the dead-pid proof | The owner of the claim does not own its heartbeat; two lifetimes | The heartbeat is the backend set's task (one per writable set, `sqz-meta` class, stopped inside `close` before the claim deletion); the per-mount `client:` refresh rides it as a callback |
| Q7 | `host_identity()` reads `/etc/nvme/*` and `get_host_id` WRITES `/etc/nvme/hostid` on first read (INIT:66-70, 87-90) | A mount silently mutating host configuration | Rung 5 never writes `/etc/nvme`; an absent default pair with no explicit knob reads as `{empty, empty}` and the stability check compares emptiness ("still absent") — the fabrics initiator (rung 8) decides minting |
| Q8 | `SQUEEZEFS_META_PR_WERO` and `SQUEEZEFS_SYM_ALLOW_NON_PR` choose a WEAKER fence posture, refused-unless-opted-in (B:18455-18516) | Charter §1.6 "the user never chooses a data path", §1.8 "never a silent fallback to a weaker posture"; a knob whose doc is "lab posture" | Delete both (rung 6): the manager holds WERO where RESCAP ≠ 0; a regular file is the single-kernel class (admitted, logged); a non-PR BLOCK device under the symmetric arm refuses loud with the nvmet remedy — no opt-in |
| Q9 | `SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS` default `45_000` beside the constant `CLIENT_STALE_TTL_SECS = 45` (env_knobs.rs:353; FC:195) | A constant with a knob on top (0002 row 17); two spellings of the one law | `T_owner` IS `STALE_TTL` (one `pub const` with its WHY, §4.3); no knob; `SKEW_MAX`/`PURGE`/`GRACE` stay derived overrides |
| Q10 | The claim's `ts` and every registration stamp are wall-clock seconds compared across hosts with no skew term (B:17379, 832-834); a host 45 s ahead preempts a live holder on PR | Charter §5.5 names `skew_max` for the custody quarantine; a wall clock steps | Keep wall seconds (the only cross-host comparable clock) but (a) classify `ts > now + skew_bound` as `Fresh` AND log "claim from the future — check this host's clock", (b) on PR, a preempt requires `age > TTL + skew_bound` (`skew_bound` = the membership derivation's `2·skew_max`, 45 ms at 500 ppm, floored at 1 s for NTP steps) — the device still fences either way |
| Q11 | `register()` probes IEKEY/CPTPL support by retrying three encodings on ANY non-conflict error (R:1099-1111) | A transport errno (disconnect) triggers two more blind retries; "targets vary" hides which shape the device accepted | Probe once at the RESCAP step: RESCAP bit 0 = PTPL capable (spec) decides `CPTPL`; retry the plain shape only on a device-answered Invalid Field (`SC 0x02`), never on an errno; record `pr_ptpl_requested` and read PTPLS (report byte 9) for the mount log |
| Q12 | `rescap()` reads 4096 B Identify into a `vec!` per call; report buffers `vec![0; len]` per transfer (R:1028, 945) | Per-op allocation on a 10 s cadence is harmless but the port's convention is pools | One page-aligned 8 KiB guard arena (Identify 4 KiB + report header/body + payload words) owned by the guard for its life |
| Q13 | The `pr_registrant_cap` learn/candidate/refusals as process-wide `static AtomicU64`s with a `BTreeMap<PathBuf,…>` under a `Mutex` for the per-namespace gauges (R:217-268) | Path-keyed string maps and globals for per-device facts | Per-`ReservationClient` fields (`regctl`, `report_bytes`, `cap_candidate`), folded on the stats read; rung 6 |
| Q14 | `claim_clear` runs a full `open_inner(Writer)` + `writer_bring_up` + checkpoint + shutdown to delete one record (B:18893-18964) | The verb replays and covers a whole volume to remove a 72 B record | Necessary on a forest (the removal's frame must be legal, B:18951-18955); on a `--single-writer` volume the same door is the only writer path — keep, but the verb is the ONE place that opens a writer without spawning the checkpoint task (the rails row) |

Not quirks (keep as designed): the flock on a dedicated fd; the claim barriered before the
checkpoint task and FUSE; no automatic cross-host takeover without device enforcement; the
register ladder's fail-closed arms; "unregister only OWN stale keys"; the two-step REGCTL
report; `preempt` refusing to arbitrate blind; `claim clear`'s refusals; the heartbeat
re-check ordering (PR before claim); `release` + `unregister` for zero residue.
