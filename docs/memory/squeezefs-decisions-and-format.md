# SqueezeFS-Zig design decisions and on-disk format
Decision records 0001–0003 (rulings §D–§R for rungs 3–10, incl. the MutualTls/kTLS wire channel), the compatibility stance, the reference-quirk ledger, format v4/v5 facts, and the runtime fixes that departed from the Rust reference.

## Compatibility stance (owner ruling)
- Do not carry poor design decisions from the Rust reference into the Zig port; no on-disk or wire compatibility with the reference is owed.
- Charter mechanisms, invariants and derivations remain requirements; the reference is evidence, not law.
- Format is forward-only among Zig releases (charter §1.7) starting at the first Zig release.
- The Rust reference `fsck --offline` oracle is RETIRED since format v4; the Zig fsck (C1–C17, rung 8 a7d2943) plus invariants and the kill matrix are the oracle.

## Hash: sqzhash128 replaces xxh3-64 — format v5 (0003 §M, 0002 row 480, landed d93d499)
- The owner's keyed AES-round 128-bit hash; `src/core/sqzhash.zig` is the pinned definition, `core/checksum.zig` the ONE algorithm-neutral facade (`hash64` = low 64 bits for every 8-byte field; `hash64Keyed` with a per-mount `Key.init(hash_seed)` for hash54/hash56; fixed public `FORMAT_KEY` for every checksum; streaming `Hasher`). Wire/page fields `xxh3_le` → `csum_le`. Mount refuses v4 loud. Also bumped data superblock v1→v2, framed image v2→3, staging record 2→3 / marker 1→2, read-cache frame 1→2. Evidence `docs/evidence/rung-09-sqzhash.md`; `hash` bench group in the baseline. Full details and numbers in the sqzhash128 topic.

## Decision records (docs/decisions/)
- `0001` — executor under `src/exec/`; §2 amended: ParkedRing is the lane's ring.
- `0002-no-compatibility-obligation.md` — ledger of reference quirks not carried. `AGENTS.md` requires adding a ledger row BEFORE diverging. Rows 10–50 quirk review; 300–355 rung 7; 434–441 rung 8; 442–482 rung 9 (480 sqzhash, 481–482 parked stamps); 483–492 rung 10 (483–485+492 symmetric completion, 486–488 operations wiring, 489–491 mTLS). Concurrent agents collide on row numbers — re-read the ledger before appending and renumber afterward.
- `0003-quirk-review-rulings.md` — ACCEPTED; §D rung 3, §E rung 6, §F rung 7, §G rung 8, §H IL arena floor, §I memory footprint, §J write amplification, §K read amplification, §L journal pad (§L.6 parked stamps), §M sqzhash, §N symmetric completion, §O operations wiring, §P MutualTls, §Q striping over the wire, §R jobs.

## 0002 ledger rows (early)
1. Byte-count CAS journal admission (ABA-blind) → monotonic `admitted_total`.
2. bincode-encoded layout-delta base → extern struct with canonical total codec.
3. Two delta classes → one Delta(field_mask, fields) kind.
4. Inode value with varint tag → fixed-width extern struct inode, size-asserted.
5. DUR-8c next_ino recovery → fold per keyspace via `inos.routeIno` (also row 49).
6. Journal lane and conveyor pass on different threads → both on one lane thread.
7. Blocking mutexes in cache hot paths → miss-path only, hot reads via ArcSwap.
8. fsck requiring the format_config xattr → judge the superblock.
9. catch_unwind panic-guarded tasks → error unions + tripwires.
- Row 48: a closing volume answers EROFS, a refused device EINVAL. Row 93: `--daemon` requires `--log-file`. Row 292: the KVDA page sink (landed in rung 10, 9fa2d0d). Row 351: the 'aio -EIO' signal (fixed 1a171ba).
- Reference FUSE codec bugs not carried: one TTL for both entry/attr in ReplyEntry; `subsec_micros()` encoded as nsec; bincode as ABI codec; positive EINVAL on INIT decode failure; second float parser for `-o *_timeout=`; notify frames with wrong `len`/missing NUL.

## Decision 0003 rulings (Q1–Q9, §D)
- Q1: aligned 16 B record / 24 B entry headers with a reserved byte; PAD = entry kind 1.
- Q2+Q3: ONE key layout; `--single-writer` is a one-member forest.
- Q4: fleet share derived from a cgroup.procs census.
- Q5: fail-stop lattice; the per-read 30 s timeout deleted.
- Q6: thread classes `sqz-fuse-ln<N>` / `sqz-fuse-ur<qid>`.
- Q7: the `.tuning` knob class deleted. Q8: dist = ReleaseSafe + fat LTO. Q9: SETATTR-echo absorber deferred to rung 3.
- §D (rung 3): lane-resident timers with per-lane heaps; fd-watch via POLL_ADD on the lane's own ring; ONE `sqz-timer` thread only for non-lane contexts; core-0 reservation derived from isolcpus/nohz_full/affinity; `Io.Batch` on a lane fails loud; first-party SQE128/CQE32 binding required. The reference's global timer thread cost a cross-thread hop measured at 59–62 % of a journal write round trip.

## Decision 0003 §E (rung 6, symmetric plane)
- Membership rows are `WriterClaimV1`-shaped records (kind = 1) in tree 0 of the home volume; `SLOT_PAGE_BUDGET = 108`.
- Wire threads `sqz-wire-acc`/`sqz-wire-conn` join the §9 naming contract; `sqz-wire-mux` is unclassified.
- The wire authenticates with an HMAC-SHA256 trailer over the `job:enroll` storage-trust secret with HKDF session keys; mTLS landed in rung 10 (§P).
- Rung 5's `KvTx.cross`/`commitSplit`/EXDEV stopgap is retired by cross-owner intents.
- The writer measures its own block rate and names `want` for grants; a share-intent witness backs the un-share protocol.

## Decision 0003 §F (rung 7, client bypass)
- Heartbeat implemented; SeqCst fetch_add + fence submit doorbell; shim `-z nodelete` via explicit `zig build-lib` Run step; shim uses raw `std.os.linux` only; total-by-construction no-panic law; SDK built from the design doc; ONE client library `src/ipc/client.zig`. Details in the client-bypass topic.

## Decision 0003 §G (rung 8, operations)
- `job:` records are tree-0 control records (tags 13/14, v4 extern structs).
- Job verbs ride the ONE listener as verb block 0x06xx; the admin lane is block 0x07xx behind the storage-trust ladder.
- nvmet is driven by writing configfs and `/dev/nvme-fabrics` directly; share ledger written by atomic rename; discovery uevent-driven.
- zcrx: 24 B header scratch, no per-op read timeout; the NIC leg is a loud probe + `Absent` posture.
- fsck: ONE tagged finding union; census over lessee shards only.
- `enable-symmetric` on a `--single-writer` set = stamp the capability bit + lay the appender region (landed rung 10, 69bf78a).
- `Ring128.initWithCq(sq, cq, flags)` with CLAMP for the zcrx lane's derived CQ.
- The ONE mover is `plane.republishBinding` (nine steps; a shared block moves once). `defrag --rekey` runs through it (rung 10, 4a4dbff, kill-at-every-step table).

## Decision 0003 §N/§O (rung 10 wave 1)
- §N symmetric completion (6ace40a, `docs/evidence/rung-10-symmetric-completion.md`): enable-symmetric, KVDA page sink, joiner-side grant ledger, handover step-0 drain, parked joiner `.stats`, `slot_tails` spill, paged dentry set. Not done: 5d dead recoverer's stash (ring-0 interior record for a slot-tree-0 lease → stash at open's second replayEntries, apply at recovery step 4) and 5e un-share.
- §O operations wiring (398f65d, `rung-10-operations-wiring.md`): daemon-owned nvmet connect at mount salted by mountpoint + `disconnect --identity-only`; config generation on `GrantHead` + `ResolveSetConfig` 0x05B5; fixed a pre-existing EIO on transformed packed tenants (`read_path.readMappingWindow` never decoded the frame). Owed: the manager's drain census walks only its own slot trees (drain over a joiner's blocks never converges); a fabric-endpoint volume added online waits for the joiner's next mount.
- Fabric multipath (rung 10, 0002 row 543, `rung-10-fabric-multipath.md`): `set_config` VERSION 2 — a data volume's `FabricEndpoint` is one subsystem over 1..=16 paths (`FabricEndpointWire` head + `FabricPathWire` × n + NQN; v1 decodes as a list of one); the daemon connects every path under one identity, accepts the kernel's multipath head only when its serving set is exactly ours (`ops/nvmet/multipath.zig`), writes `SQUEEZEFS_FABRIC_IOPOLICY` (default `round-robin`, owner ruling 2026-10-18: aggregate bandwidth over two 200 GbE links; queue-depth leaves a lone stream on one link) to every owned subsystem incl. the operator-connected metadata head (rule 2 per path), and connects with `fast_io_fail_tmo 45 / ctrl_loss_tmo 90 / reconnect_delay 10` (the claim's clock). `nvmeof status` lists heads/policies/paths. Follow-up: a `sqmeta://` fabric URI so the daemon owns the metadata connects too.

## Decision 0003 §P — MutualTls wire channel class (rung 10, landed 1afe669; 0002 rows 489–491)
- Owner direction 2026-10-12: handshake in the in-tree TLS 1.3 engine over std.crypto (TLS_AES_128_GCM_SHA256 + X25519 + Ed25519, exporter-bound HMAC session key), record layer on Linux = kernel TLS: traffic keys handed to the kernel via `TCP_ULP "tls"` + `TLS_TX`/`TLS_RX` (`tls12_crypto_info_aes_gcm_128` with `TLS_1_3_VERSION`); frames then ride the same path as Plaintext.
- On Linux it is kTLS or the `mutual_tls` mount fails loud naming the missing module; a CA-less or ULP-less mount refuses by name; no userspace fallback (no two Linux record layers). The userspace record layer survives only as the portable/macOS test path and the handshake transport.
- Non-application records (alerts, KeyUpdate, NewSessionTicket) arrive as `TLS_GET_RECORD_TYPE` cmsgs, so RX uses recvmsg and dispatches control records in the mux. `SessionAuthn.confidential()` is true only when both TLS_TX and TLS_RX are installed. TLS 1.3 KeyUpdate via kTLS needs Linux ≥ 6.14 (re-setsockopt both halves); a KeyUpdate sender must rotate TX immediately or the kernel answers EKEYEXPIRED; every record must be received whole into the Conn scratch (a control record recvmsg'd into a too-small buffer is split and the remainder unreadable).
- zcrx (RECV_ZC) is incompatible with software kTLS (delivers raw TCP pages); only NIC TLS RX offload (CONFIG_TLS_DEVICE) combines — hardware-owed; the zcrx lane must refuse (counted) on MutualTls without device offload.
- The wire frame path stays on blocking syscalls (Plaintext by spec §1.1); moving both classes to IORING_OP_RECVMSG/SEND_ZC is recorded as the owner's call in §P.
- PKI: in-tree Ed25519, `squeezefs key tls-init` / `tls-issue`, minimal X.509 v3 DER/PEM; `pki.Screen` guards `std.crypto.Certificate.parse` (panics on short input). OpenSSL interop tests both roles. Env: `SQUEEZEFS_WIRE_CHANNEL`, `SQUEEZEFS_WIRE_TLS_{CA,CERT,KEY}`.
- Measured (ReleaseSafe, loopback): plaintext 19.2 GB/s @0.10 CPU-s/GB; userspace TLS 2.04 GB/s @0.94; kTLS without ARM64 CE crypto drivers 0.41 GB/s @4.27 (kernel `gcm(aes)` = `gcm_base(ctr(aes-lib),ghash-lib)`); kTLS with `gcm-aes-ce` 3.5–3.7 GB/s @0.47–0.50 (64 KiB) — 2× less CPU than userspace; the remaining gap to plaintext is the AEAD pass (NIC TLS offload only).
- Also documented in `docs/porting/sym_wire_membership_recovery.md` and `docs/evidence/rung-10-mtls.md`.

## Kernel deltas — owner ruling 2026-10-13: keep them (0003 §Z.6, commit 9091b86)
- The sqz series' 26 patches were already required by the Rust reference (kmbuf rings, FUSE zero-copy over io_uring, zc payload retention, FUSE_TIME_LIMITS, host-scoped nvme fabric subsystems, per-queue bg accounting). This implementation adds 0027 (`FUSE_HAS_SYNCFS` backport from fuse for-next — makes `sync`/`syncfs` a durability point for the daemon's write custody; the Rust reference has the same gap, unmeasured) and 0028 (our fix: the FUSE-uring abort never ended requests queued for a stopped server; latent for any FUSE-uring server — owed upstream). kTLS and ARMv8 CE crypto were config-only. The syncfs-off knob was offered and rejected.
- When the owner asks why the kernel was patched: lead with the existing 26-patch series, then the delta, its measured reason, and a gain/cost table.

## Format v4 (commit d33e825) and v5 (d93d499)
- v4: 192 B `SuperblockWire`, checksum last, `volume_len`, capability bits SYMMETRIC_PLANE / LAYOUT_DELTAS / BLOCK_MAP_TREE / VOLUME_LIFECYCLE; independent random `hash_seed` via `Io.randomSecure`.
- ONE key layout with the kind byte: inode 9 B, dentry/xattr 17 B, map 13 B, refs 29 B.
- Fixed-width 68 B `InodeValueWire` extern struct with a comptime-derived field table; one 40 B stamped bset frame; versioned fixed ledger layout; ASCII magics.
- Journal page header `{magic, lap, first_entry_off, reserved, appender_id u32, csum}`.
- Derived ties: fold capacity 257,976 / 61,368 (256/64 KiB); interior record 41 B ⇒ fanout 4,719; ref separator 49 B; maxPad 4120 @4096; worst-case ledger record 3,767 B.
- `tree.TreeIdentity{control, slot}`, `ledger.RootId{control=8, native_slot=0x0100}`; `tree.route` is THE routing law.
- v5 = v4 with every checksum field sqzhash128-low-64 under FORMAT_KEY and name hashes keyed per volume; fixture `tests/fixtures/findvsa_pred_bset.v5.bin`.

## Runtime fixes from the quirk review (WP-R, 31aea2b..bd1c634)
- Bugs the kill matrix had not caught: threshold writeback never ran on a mounted volume; SMO pre-lock barrier failure swallowed by `catch {}`; the lane's barriers never advanced DUR-3 `reusable_upto`; space wake split so the 10 ms tick was load-bearing; rollback OOM exits silent.
- `DEFAULT_FLUSH_INTERVAL_MS = CHECKPOINT_MAX_AGE_MS / 20 = 50`; `SQUEEZEFS_META_FLUSH_INTERVAL_MS` is the A/B lever, 0 = strict.
- `heap_law.HeapFullPosture` is THE heap_full posture; inflight map pre-sized; nvme_dev reads wait unbounded with an overdue census.
- Hooks `Checkpointer.barrierStarted/barrierLanded`; counters `meta_conveyor_park_tick_rescues`, `meta_kv_smo_barrier_failures`, `conveyor_divergence_{rollout,rollback,compensation}`.